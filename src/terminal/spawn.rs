//! Terminal launch orchestration, process spawning, and Warp helpers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{
    TERMINAL_CONTEXT_VARS, TerminalCommandContext, create_bash_script, create_powershell_script,
    get_linux_terminal_argv, get_macos_terminal_argv, get_windows_terminal_argv,
    is_native_termux_runtime, normalize_terminal_mode_for_launch, proot_termux_launch_error,
    resolve_bash_command, resolve_terminal_open_argv, shell_quote, should_use_command_extension,
    substitute_open_argv, termux_host_visible, validate_termux_dispatch_status,
    windows_shellify_custom_argv,
};
use crate::paths;
use crate::shared::constants::HCOM_IDENTITY_VARS;
use crate::shared::platform;
use crate::shared::tool_detection::tool_marker_vars;

#[derive(Debug)]
pub enum LaunchResult {
    /// Background mode: (log_file_path, pid)
    Background(String, u32),
    /// Success (run_here or new window)
    Success,
    /// Failed
    Failed(String),
}

/// macOS app bundle fallback commands for cross-platform terminals.
/// Used when CLI binary isn't in PATH but .app bundle is installed.
fn get_launcher_env() -> HashMap<String, String> {
    get_launcher_env_from(std::env::vars())
}

fn get_launcher_env_from<I>(vars: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut strip: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for v in tool_marker_vars() {
        strip.insert(v);
    }
    for v in HCOM_IDENTITY_VARS {
        strip.insert(v);
    }
    for v in TERMINAL_CONTEXT_VARS {
        strip.insert(v);
    }
    strip.insert("HCOM_LAUNCHED_PRESET");

    vars.into_iter()
        .filter(|(k, _)| !strip.contains(k.as_str()))
        .collect()
}

/// Inputs to terminal command template substitution.
///
/// All fields are borrowed and may be empty; unknown placeholders are left
/// in place by `substitute_open_argv` (no substitution panics).
fn yaml_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Warp Stable's `~/.warp/launch_configurations/` dir.
///
/// Stable only for v1. Other channels (Preview/Dev/Local/Oss) own separate
/// config dirs and URL schemes (`warppreview://` etc.), so a single `warp://`
/// only ever reaches one channel. Add per-channel presets later if needed.
fn warp_launch_config_dir(home: &Path) -> PathBuf {
    home.join(".warp").join("launch_configurations")
}

/// Build YAML body for a one-pane Warp launch config that runs `bash <script>`.
fn build_warp_launch_yaml(config_name: &str, cwd: &str, script: &str) -> String {
    let exec_str = format!("bash {}", shell_quote(script));
    format!(
        "name: {name}\nwindows:\n  - tabs:\n      - layout:\n          cwd: {cwd}\n          commands:\n            - exec: {exec}\n",
        name = yaml_double_quote(config_name),
        cwd = yaml_double_quote(cwd),
        exec = yaml_double_quote(&exec_str),
    )
}

/// Resolve `cwd` to an absolute path Warp will accept for the pane's initial dir.
///
/// Warp's URL-based launch decouples the pane from the spawning process's
/// working dir, so the pane cwd must be set explicitly. For relative or
/// missing input, use the launcher's current_dir (the prefix the script's
/// later `cd <cwd>` would resolve against) so a relative `cd .` or
/// `cd subdir` lands where a non-Warp launch would. HOME is a last resort
/// if current_dir() fails.
fn resolve_warp_cwd(cwd: Option<&str>, home: &Path) -> String {
    if let Some(c) = cwd
        && Path::new(c).is_absolute()
    {
        return c.to_string();
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| home.to_string_lossy().to_string())
}

/// Delete hcom-*.yaml files older than `older_than` from a channel dir.
///
/// Sweep-on-write avoids races with Warp cold start (where `open warp://...`
/// returns before Warp boots and reads the URL). Older configs should no
/// longer be needed by Warp.
const WARP_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

fn sweep_stale_warp_configs(dir: &Path, older_than: std::time::Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("hcom-") || !name_str.ends_with(".yaml") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if now.duration_since(mtime).unwrap_or_default() > older_than {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Write a Warp launch_config YAML for `bash <script>` to Warp Stable's dir.
///
/// Warp has no CLI inject; the only way to launch a command is via
/// `warp://launch/<config_name>` which reads a YAML from the channel-specific
/// `launch_configurations/` dir. Returns the path written.
fn write_warp_launch_config(process_id: &str, cwd: Option<&str>, script: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    write_warp_launch_config_at(Path::new(&home), process_id, cwd, script)
}

fn write_warp_launch_config_at(
    home: &Path,
    process_id: &str,
    cwd: Option<&str>,
    script: &str,
) -> Result<PathBuf> {
    let dir = warp_launch_config_dir(home);
    fs::create_dir_all(&dir).context("Failed to create Warp launch_configurations dir")?;
    sweep_stale_warp_configs(&dir, WARP_STALE_AFTER);

    let config_name = format!("hcom-{}", process_id);
    let resolved_cwd = resolve_warp_cwd(cwd, home);
    let yaml = build_warp_launch_yaml(&config_name, &resolved_cwd, script);
    let yaml_path = dir.join(format!("{}.yaml", config_name));
    fs::write(&yaml_path, &yaml).context("Failed to write Warp launch config")?;
    Ok(yaml_path)
}

/// Human-readable name for the Windows default-terminal fallback, mirroring
/// `windows_default_terminal_template`'s `has_wt` branch.
fn is_external_terminal_launcher(argv: &[String]) -> bool {
    let Some(cmd) = argv.first().map(|s| s.as_str()) else {
        return false;
    };

    matches!(
        cmd,
        "open"
            | "tmux"
            | "kitty"
            | "kitten"
            | "wezterm"
            | "alacritty"
            | "cmux"
            | "ttab"
            | "wttab"
            | "gnome-terminal"
            | "konsole"
            | "xterm"
            | "tilix"
            | "terminator"
            | "zellij"
            | "wsh"
            | "wt"
            | "wt.exe"
            | "mintty"
            | "herdr"
    )
}

fn maybe_append_ai_tool_launch_hint(
    message: String,
    argv: &[String],
    inside_ai_tool: bool,
) -> String {
    if !inside_ai_tool || !is_external_terminal_launcher(argv) {
        return message;
    }

    let hint = if argv.first().map(|s| s.as_str()) == Some("tmux") {
        "Hint: Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
    } else {
        "Hint: This launch was attempted from inside a managed AI tool session, where terminal or session creation may be restricted. Rerun it with approval/escalation."
    };
    format!("{message}\n{hint}")
}

fn zellij_action_stderr_failure(argv: &[String], stderr: &str) -> Option<String> {
    if argv.first().map(|s| s.as_str()) != Some("zellij") {
        return None;
    }

    let stderr = stderr.trim();
    if stderr.contains("Please specify the session name to send actions to") {
        return Some(stderr.to_string());
    }

    None
}

fn validate_terminal_launch_output(
    argv: &[String],
    output: &std::process::Output,
    inside_ai_tool: bool,
) -> Result<()> {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let msg = format!(
            "Terminal launch failed (exit code {})",
            output.status.code().unwrap_or(-1)
        );
        let full_msg = if stderr.is_empty() {
            msg
        } else {
            format!("{}: {}", msg, stderr)
        };
        bail!(maybe_append_ai_tool_launch_hint(
            full_msg,
            argv,
            inside_ai_tool
        ));
    }

    if let Some(msg) = zellij_action_stderr_failure(argv, &stderr) {
        bail!(maybe_append_ai_tool_launch_hint(
            format!("Terminal launch failed: {msg}"),
            argv,
            inside_ai_tool
        ));
    }

    Ok(())
}

fn spawn_terminal_process(argv: &[String], inside_ai_tool: bool) -> Result<(bool, String)> {
    let launcher_env = get_launcher_env();
    let env_vec: Vec<(String, String)> = launcher_env.into_iter().collect();

    #[cfg(windows)]
    if argv.first().is_some_and(|arg| {
        Path::new(arg)
            .file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("wezterm"))
    }) && argv.get(1).is_some_and(|arg| arg == "start")
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|err| {
                anyhow!(maybe_append_ai_tool_launch_hint(
                    format!("Failed to spawn terminal process: {err}"),
                    argv,
                    inside_ai_tool,
                ))
            })?;
        return Ok((true, String::new()));
    }

    if inside_ai_tool {
        // Fully detach: don't let AI tool's PTY capture our output
        let launch_dir = paths::hcom_path(&[paths::LAUNCH_DIR]);
        fs::create_dir_all(&launch_dir).ok();

        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| {
                anyhow!(maybe_append_ai_tool_launch_hint(
                    format!("Failed to spawn terminal process: {err}"),
                    argv,
                    inside_ai_tool,
                ))
            })?;

        let output = child
            .wait_with_output()
            .context("Failed to wait for terminal")?;

        let captured = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();

        validate_terminal_launch_output(argv, &output, inside_ai_tool)?;

        Ok((true, captured))
    } else {
        // Normal case: wait for terminal launcher to complete
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .context("Failed to run terminal launcher")?;

        validate_terminal_launch_output(argv, &output, inside_ai_tool)?;

        let captured = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        Ok((true, captured))
    }
}

/// Write captured terminal ID to temp file for child to read.
fn write_terminal_id(env: &HashMap<String, String>, captured_id: &str) {
    let captured_id = normalize_captured_terminal_id(captured_id);
    if captured_id.is_empty() {
        return;
    }
    let process_id = match env.get("HCOM_PROCESS_ID") {
        Some(pid) if !pid.is_empty() => pid,
        _ => return,
    };
    let ids_dir = paths::hcom_path(&[".tmp", "terminal_ids"]);
    fs::create_dir_all(&ids_dir).ok();
    fs::write(ids_dir.join(process_id), captured_id).ok();
}

fn normalize_captured_terminal_id(captured_id: &str) -> String {
    let captured_id = captured_id.trim();

    // Herdr: parse JSON response and extract result.agent.pane_id
    if let Some(pane_id) = parse_herdr_pane_id(captured_id) {
        return pane_id;
    }

    // Waveterm: "run block created: block:abc123" -> "block:abc123"
    let Some((_, block_ref)) = captured_id.rsplit_once("block:") else {
        return captured_id.to_string();
    };
    let block_id = block_ref.split_whitespace().next().unwrap_or("");
    if block_id.is_empty() {
        captured_id.to_string()
    } else {
        format!("block:{block_id}")
    }
}

/// Parse herdr `agent start` JSON output and extract `result.agent.pane_id`.
///
/// Gated on the response `id` field (`cli:agent:start`) so non-herdr terminal
/// outputs that happen to be JSON don't accidentally match this shape.
fn parse_herdr_pane_id(captured: &str) -> Option<String> {
    let trimmed = captured.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if val.get("id").and_then(|v| v.as_str()) != Some("cli:agent:start") {
        return None;
    }
    val.get("result")?
        .get("agent")?
        .get("pane_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Launch terminal with command.
///
/// # Modes
/// - `background=true`: Launch as background process, returns Background(log_file, pid)
/// - `run_here=true`: Run in current terminal (blocking via execve)
/// - Otherwise: New terminal window/tab/split
pub fn launch_terminal(
    command: &str,
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    background: bool,
    run_here: bool,
    terminal: Option<&str>,
    inside_ai_tool: bool,
) -> Result<(LaunchResult, String)> {
    let config_and_instance_env = env.clone();

    // Determine terminal mode
    let mut terminal_mode = terminal.unwrap_or("default").to_string();

    let opens_new_window = !background && !run_here;

    // Resolve smart terminal shortcuts
    let (terminal_mode_resolved, kitty_socket) =
        normalize_terminal_mode_for_launch(terminal_mode, opens_new_window, run_here);
    terminal_mode = terminal_mode_resolved;

    let mut final_env = config_and_instance_env;
    if opens_new_window && !kitty_socket.is_empty() {
        final_env.insert("KITTY_LISTEN_ON".to_string(), kitty_socket.clone());
    }

    if terminal_mode != "default" && terminal_mode != "print" {
        final_env.insert("HCOM_LAUNCHED_PRESET".to_string(), terminal_mode.clone());
    }

    // Determine script extension after terminal mode resolution so explicit
    // Terminal.app uses the macOS `.command` launcher just like auto-detect.
    let extension = if cfg!(windows) {
        ".ps1"
    } else if should_use_command_extension(background, &terminal_mode) {
        ".command"
    } else {
        ".sh"
    };
    let script_file = paths::hcom_path(&[
        paths::LAUNCH_DIR,
        &format!(
            "hcom_{}_{}{}",
            std::process::id(),
            rand::random::<u16>() % 9000 + 1000,
            extension
        ),
    ]);

    // Ensure launch dir exists
    if let Some(parent) = script_file.parent() {
        fs::create_dir_all(parent).ok();
    }

    // Create script. Windows uses a native PowerShell script; Unix uses bash.
    if cfg!(windows) {
        create_powershell_script(
            &script_file,
            &final_env,
            cwd,
            command,
            background,
            None,
            opens_new_window,
        )?;
    } else {
        create_bash_script(
            &script_file,
            &final_env,
            cwd,
            command,
            background,
            None,
            opens_new_window,
        )?;
    }

    // Background mode
    if background {
        let logs_dir = paths::hcom_path(&[paths::LOGS_DIR]);
        fs::create_dir_all(&logs_dir).ok();
        let log_name = env.get("HCOM_BACKGROUND").cloned().unwrap_or_default();
        let log_file = logs_dir.join(&log_name);

        let log_handle = fs::File::create(&log_file).context("Failed to create log file")?;

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-ExecutionPolicy", "Bypass", "-File"]);
            c
        } else {
            Command::new(resolve_bash_command())
        };
        cmd.arg(&script_file)
            .stdin(std::process::Stdio::null())
            .stdout(log_handle.try_clone()?)
            .stderr(log_handle);

        // Detach child into its own session so it survives parent exit (no
        // SIGHUP), without leaking a captured caller's stdout/stderr handles
        // into the long-lived runner on Windows.
        let child = crate::sys::process::spawn_detached(&mut cmd)
            .context("Failed to launch background process")?;

        // Brief health check
        std::thread::sleep(std::time::Duration::from_millis(200));
        let pid = child.id();

        return Ok((
            LaunchResult::Background(log_file.to_string_lossy().to_string(), pid),
            terminal_mode,
        ));
    }

    // Print mode (debug)
    if terminal_mode == "print" {
        let content = fs::read_to_string(&script_file)?;
        println!("# Script: {}", script_file.display());
        print!("{}", content);
        fs::remove_file(&script_file).ok();
        return Ok((LaunchResult::Success, terminal_mode));
    }

    // Run in current terminal (blocking)
    if run_here {
        // Build full env (config + shell)
        let full_env = build_full_env(&final_env);
        if let Some(dir) = cwd {
            std::env::set_current_dir(dir).ok();
        }
        // Replace this process entirely with the script's shell.
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-ExecutionPolicy", "Bypass", "-File"]);
            c
        } else {
            Command::new(resolve_bash_command())
        };
        cmd.arg(script_file).env_clear().envs(&full_env);
        let err = crate::sys::process::exec_replace(cmd);
        bail!("exec failed: {}", err);
    }

    // New window / custom command mode
    let custom_cmd: Option<Vec<String>> = if terminal_mode == "default" {
        None
    } else if crate::config::get_merged_preset(&terminal_mode).is_some() {
        // Built-in presets not available on this platform are rejected here too
        // (not just at config-validation time) so HCOM_TERMINAL can't bypass the
        // check. User-defined TOML presets declare no platform and are exempt.
        if crate::config::is_known_terminal_preset_pub(&terminal_mode)
            && !crate::config::is_user_defined_preset(&terminal_mode)
            && !crate::config::terminal_preset_supported_on(
                &terminal_mode,
                platform::platform_name(),
            )
        {
            bail!(
                "terminal preset '{}' is not available on {}",
                terminal_mode,
                platform::platform_name()
            );
        }
        // Known preset — check kitty remote control requirements
        if terminal_mode == "kitty-tab" || terminal_mode == "kitty-split" {
            let listen_on = std::env::var("KITTY_LISTEN_ON")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| kitty_socket.clone());
            if listen_on.is_empty() {
                bail!(
                    "{} requires remote control.\n\
                     Add to ~/.config/kitty/kitty.conf:\n\
                     allow_remote_control yes\n\
                     listen_on unix:/tmp/kitty\n\
                     Then restart kitty.",
                    terminal_mode
                );
            }
        }
        let mut argv = resolve_terminal_open_argv(&terminal_mode).unwrap_or_default();
        // Inject `--to <socket>` (after the `@`) for kitten commands launched
        // outside kitty. Splice as separate argv elements — no shell quoting.
        if !kitty_socket.is_empty()
            && !kitty_socket.starts_with("fd:")
            && argv.first().map(String::as_str) == Some("kitten")
            && argv.get(1).map(String::as_str) == Some("@")
            && !argv.iter().any(|a| a == "--to")
        {
            argv.splice(2..2, ["--to".to_string(), kitty_socket.clone()]);
        }
        // Target launcher's tab for splits: insert `--match window_id:<wid>`
        // before the `--` separator.
        if (terminal_mode == "kitty-tab" || terminal_mode == "kitty-split")
            && let Ok(wid) = std::env::var("KITTY_WINDOW_ID")
            && !wid.is_empty()
            && let Some(sep) = argv.iter().position(|a| a == "--")
        {
            argv.splice(
                sep..sep,
                ["--match".to_string(), format!("window_id:{wid}")],
            );
        }
        Some(argv)
    } else {
        // Custom command template string (HCOM_TERMINAL / config custom command).
        // Tokenize once via the double-quote-aware splitter; the array-form TOML
        // preset path never reaches here (those are known presets).
        //
        // `shell_split` itself treats an unquoted `\` as a literal character on
        // Windows (instead of a POSIX escape), so Windows paths like
        // `C:\Tools\term.exe` supplied via HCOM_TERMINAL survive intact without
        // any pre-processing here.
        let argv = match crate::tools::args_common::shell_split(&terminal_mode, cfg!(windows)) {
            Ok(argv) if !argv.is_empty() => argv,
            Ok(_) => bail!("custom terminal command is empty"),
            Err(e) => bail!("invalid quoting in custom terminal command: {e}"),
        };
        Some(windows_shellify_custom_argv(argv)?)
    };

    let script_str = script_file.to_string_lossy().to_string();

    if terminal_mode == "warp" {
        let process_id = env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or("");
        if process_id.is_empty() {
            bail!("warp preset requires HCOM_PROCESS_ID to name the launch config");
        }
        write_warp_launch_config(process_id, cwd, &script_str)?;
        let final_argv = vec![
            "open".to_string(),
            format!("warp://launch/hcom-{}", process_id),
        ];
        let (success, captured_id) = spawn_terminal_process(&final_argv, inside_ai_tool)?;
        write_terminal_id(env, &captured_id);
        return if success {
            Ok((LaunchResult::Success, terminal_mode))
        } else {
            Ok((
                LaunchResult::Failed("Terminal process failed".to_string()),
                terminal_mode,
            ))
        };
    }

    if let Some(cmd_template) = custom_cmd {
        // {instance_name} falls back to process_id so presets that label panes
        // (e.g. herdr) never produce an empty `[tool]-` suffix when invoked
        // outside the normal launch flow.
        let process_id = env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or("");
        let instance_name = env
            .get("HCOM_INSTANCE_NAME")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(process_id);
        let ctx = TerminalCommandContext {
            script: &script_str,
            process_id,
            cwd: cwd.unwrap_or(""),
            instance_name,
            tool: env
                .get("HCOM_TOOL")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("hcom"),
            // launcher::launch pre-formats the title; here we only read it.
            pane_title: env
                .get("HCOM_PANE_TITLE")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty()),
        };
        // The Windows `.ps1`-via-PowerShell variant is already selected by the
        // preset's `open_argv(cfg!(windows))`; no text rewrite needed.
        let final_argv = substitute_open_argv(&cmd_template, ctx)?;
        let (success, captured_id) = spawn_terminal_process(&final_argv, inside_ai_tool)?;
        write_terminal_id(env, &captured_id);
        if success {
            Ok((LaunchResult::Success, terminal_mode))
        } else {
            Ok((
                LaunchResult::Failed("Terminal process failed".to_string()),
                terminal_mode,
            ))
        }
    } else {
        // Platform default
        if termux_host_visible() {
            if !is_native_termux_runtime() {
                bail!("{}", proot_termux_launch_error());
            }

            let am_argv = vec![
                "am",
                "startservice",
                "--user",
                "0",
                "-n",
                "com.termux/com.termux.app.RunCommandService",
                "-a",
                "com.termux.RUN_COMMAND",
                "--es",
                "com.termux.RUN_COMMAND_PATH",
                &script_str,
                "--ez",
                "com.termux.RUN_COMMAND_BACKGROUND",
                "false",
            ];
            let status = Command::new(am_argv[0])
                .args(&am_argv[1..])
                .status()
                .context("Failed to launch Termux")?;
            validate_termux_dispatch_status(status)?;
            return Ok((LaunchResult::Success, terminal_mode));
        }

        let argv = match platform::platform_name() {
            "Darwin" => substitute_open_argv(
                &get_macos_terminal_argv(),
                TerminalCommandContext {
                    script: &script_str,
                    process_id: env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or(""),
                    cwd: cwd.unwrap_or(""),
                    instance_name: env
                        .get("HCOM_INSTANCE_NAME")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    tool: env.get("HCOM_TOOL").map(|s| s.as_str()).unwrap_or(""),
                    pane_title: None,
                },
            )?,
            "Linux" => get_linux_terminal_argv()
                .ok_or_else(|| anyhow::anyhow!("No supported terminal emulator found"))?,
            "Windows" => get_windows_terminal_argv(),
            other => bail!("Unsupported platform: {}", other),
        };

        let final_argv: Vec<String> = if platform::platform_name() == "Darwin" {
            argv
        } else {
            // Linux/Windows defaults carry only `{script}` placeholders.
            argv.iter()
                .map(|a| a.replace("{script}", &script_str))
                .collect()
        };
        let (success, captured_id) = spawn_terminal_process(&final_argv, inside_ai_tool)?;
        write_terminal_id(env, &captured_id);
        if success {
            Ok((LaunchResult::Success, terminal_mode))
        } else {
            Ok((
                LaunchResult::Failed("Terminal process failed".to_string()),
                terminal_mode,
            ))
        }
    }
}

/// Build full env from config env + shell env.
fn build_full_env(config_env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut full = config_env.clone();
    for (k, v) in std::env::vars() {
        if tool_marker_vars().contains(&k.as_str()) {
            continue;
        }
        if k == "HCOM_TERMINAL" {
            continue;
        }
        // Config env takes precedence for HCOM_ vars
        full.entry(k).or_insert(v);
    }
    full
}

/// Close terminal pane via preset-specific command.
///
/// Must run before SIGTERM because terminal CLIs match panes by PID/pane_id.
/// Non-fatal: caller should always proceed with SIGTERM regardless.

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;

    #[test]
    fn test_launcher_env_preserves_zellij_session_but_strips_pane() {
        let env = get_launcher_env_from(vec![
            (
                "ZELLIJ_SESSION_NAME".to_string(),
                "wise-kangaroo".to_string(),
            ),
            ("ZELLIJ_PANE_ID".to_string(), "18".to_string()),
            ("HCOM_LAUNCHED_PRESET".to_string(), "zellij".to_string()),
            ("PATH".to_string(), "/bin".to_string()),
        ]);

        assert_eq!(
            env.get("ZELLIJ_SESSION_NAME").map(String::as_str),
            Some("wise-kangaroo")
        );
        assert!(!env.contains_key("ZELLIJ_PANE_ID"));
        assert!(!env.contains_key("HCOM_LAUNCHED_PRESET"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
    }

    #[test]
    fn test_launcher_env_keeps_herdr_socket_path() {
        let env = get_launcher_env_from(vec![
            ("HERDR_SOCKET_PATH".into(), "/tmp/herdr.sock".into()),
            ("PATH".into(), "/bin".into()),
        ]);
        assert_eq!(
            env.get("HERDR_SOCKET_PATH").map(String::as_str),
            Some("/tmp/herdr.sock"),
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_zellij_session_ambiguity_stderr_fails_launch_even_with_exit_zero() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: b"Please specify the session name to send actions to. The following sessions are active:\n".to_vec(),
        };

        let err = validate_terminal_launch_output(
            &[
                "zellij".to_string(),
                "action".to_string(),
                "new-pane".to_string(),
            ],
            &output,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("Terminal launch failed"));
        assert!(err.contains("Please specify the session name"));
    }

    #[test]
    fn test_yaml_double_quote_escapes_backslash_and_quote() {
        assert_eq!(yaml_double_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(yaml_double_quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(yaml_double_quote("plain"), "\"plain\"");
    }

    #[test]
    fn test_build_warp_launch_yaml_shape() {
        let yaml = build_warp_launch_yaml("hcom-pid", "/some/dir", "/tmp/script.sh");
        assert!(yaml.contains("name: \"hcom-pid\""));
        assert!(yaml.contains("cwd: \"/some/dir\""));
        assert!(yaml.contains("exec: \"bash /tmp/script.sh\""));
    }

    #[test]
    fn test_warp_launch_config_dir_is_stable_channel() {
        let dir = warp_launch_config_dir(Path::new("/h"));
        assert_eq!(dir, Path::new("/h/.warp/launch_configurations"));
    }

    #[cfg(unix)]
    #[test]
    fn test_write_warp_launch_config_writes_to_stable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let written = write_warp_launch_config_at(
            tmp.path(),
            "test-pid",
            Some("/some/dir"),
            "/tmp/script.sh",
        )
        .unwrap();
        assert!(written.ends_with(".warp/launch_configurations/hcom-test-pid.yaml"));
        let content = std::fs::read_to_string(&written).unwrap();
        assert!(content.contains("name: \"hcom-test-pid\""));
        assert!(content.contains("exec: \"bash /tmp/script.sh\""));
        assert!(content.contains("cwd: \"/some/dir\""));
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_warp_cwd_keeps_absolute() {
        let home = Path::new("/h");
        assert_eq!(resolve_warp_cwd(Some("/abs/path"), home), "/abs/path");
    }

    #[test]
    #[serial]
    fn test_resolve_warp_cwd_uses_current_dir_for_relative_or_missing() {
        let home = Path::new("/h");
        let cwd_str = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(resolve_warp_cwd(Some("subdir"), home), cwd_str);
        assert_eq!(resolve_warp_cwd(Some("./rel"), home), cwd_str);
        assert_eq!(resolve_warp_cwd(Some("."), home), cwd_str);
        assert_eq!(resolve_warp_cwd(None, home), cwd_str);
    }

    #[test]
    fn test_sweep_stale_warp_configs_only_removes_hcom_prefixed_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let target = dir.join("hcom-old.yaml");
        let other = dir.join("user-config.yaml");
        let unrelated = dir.join("hcom-old.txt");
        std::fs::write(&target, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        std::fs::write(&unrelated, "x").unwrap();

        sweep_stale_warp_configs(dir, std::time::Duration::from_secs(0));

        assert!(!target.exists(), "hcom-*.yaml should be swept");
        assert!(other.exists(), "non-hcom-prefixed yaml should remain");
        assert!(unrelated.exists(), "non-yaml extension should remain");
    }

    #[test]
    fn test_sweep_stale_warp_configs_keeps_fresh_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let fresh = dir.join("hcom-new.yaml");
        std::fs::write(&fresh, "x").unwrap();

        sweep_stale_warp_configs(dir, std::time::Duration::from_secs(3600));

        assert!(fresh.exists(), "fresh file should remain");
    }

    #[test]
    fn test_maybe_append_ai_tool_launch_hint_for_tmux() {
        let message = maybe_append_ai_tool_launch_hint(
            "Terminal launch failed (exit code 1): permission denied".to_string(),
            &["tmux".to_string(), "new-session".to_string()],
            true,
        );
        assert!(message.contains("tmux kill-server"));
        assert!(message.contains("tmux new-session -d -s hcom-external"));
    }

    #[test]
    fn test_maybe_append_ai_tool_launch_hint_for_wsh() {
        let message = maybe_append_ai_tool_launch_hint(
            "Failed to spawn terminal process: operation not permitted".to_string(),
            &["wsh".to_string(), "launch".to_string()],
            true,
        );
        assert!(message.contains("managed AI tool session"));
        assert!(message.contains("Rerun it with approval/escalation."));
    }

    #[test]
    fn test_maybe_append_ai_tool_launch_hint_skips_non_terminal_commands() {
        let message = maybe_append_ai_tool_launch_hint(
            "plain failure".to_string(),
            &["bash".to_string()],
            true,
        );
        assert_eq!(message, "plain failure");
    }

    #[test]
    fn test_normalize_waveterm_run_block_stdout() {
        assert_eq!(
            normalize_captured_terminal_id("run block created: block:abc123\n"),
            "block:abc123"
        );
        assert_eq!(normalize_captured_terminal_id("terminal_6"), "terminal_6");
    }

    #[test]
    fn test_normalize_herdr_agent_start_json() {
        let json = r#"{"id":"cli:agent:start","result":{"agent":{"agent_status":"unknown","cwd":"/tmp","focused":false,"name":"hcom-abc123","pane_id":"w123abc-3","revision":0,"tab_id":"w123abc:2","terminal_id":"term_abc","workspace_id":"w123abc"},"argv":["bash","/tmp/script.sh"],"type":"agent_started"}}"#;
        assert_eq!(normalize_captured_terminal_id(json), "w123abc-3");
    }

    #[test]
    fn test_normalize_herdr_error_json_falls_through() {
        let json = r#"{"error":{"code":"server_unavailable","message":"herdr server not running"},"id":"cli:agent:start"}"#;
        assert_eq!(normalize_captured_terminal_id(json), json);
    }

    #[test]
    fn test_normalize_herdr_empty_pane_id() {
        let json = r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":""}}}"#;
        assert_eq!(normalize_captured_terminal_id(json), json);
    }
}
