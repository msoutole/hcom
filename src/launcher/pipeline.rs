use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, bail};
use rand::RngExt;
use serde_json::json;

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::instance_binding;
use crate::instance_names;
use crate::instances;
use crate::launcher::{LaunchBackend, LaunchParams, LaunchResult, LaunchTool};
use crate::paths;
use crate::shared::constants::HCOM_IDENTITY_VARS;
use crate::shared::tool_detection::tool_marker_vars;
use crate::terminal;
use crate::tools::codex_preprocessing;
use crate::tools::copilot_preprocessing;
use crate::tools::cursor_preprocessing;
use crate::tools::opencode_preprocessing;

use super::batch;
use super::environment::{
    build_launch_env, is_tool_installed, isolated_tool_config_dir, launch_env_regime,
    will_run_in_current_terminal, write_system_prompt_file,
};
use super::preprocess::{
    append_initial_prompt_args, inject_omp_extension_args, inject_workspace_trust_args,
    validate_launch_count, validate_tool_args,
};

/// Generate a UUID v4-like process ID string.
fn generate_process_id() -> String {
    let mut rng = rand::rng();
    let a: u32 = rng.random();
    let b: u16 = rng.random();
    let c: u16 = (rng.random::<u16>() & 0x0FFF) | 0x4000; // version 4
    let d: u16 = (rng.random::<u16>() & 0x3FFF) | 0x8000; // variant 1
    let e: u64 = rng.random::<u64>() & 0xFFFFFFFFFFFF; // 48 bits
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a, b, c, d, e)
}

fn install_diag_context(tool: &LaunchTool, paths: &[(&str, std::path::PathBuf)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Diagnostic context:");
    for (label, p) in paths {
        let _ = writeln!(out, "  resolved {label}={}", p.display());
    }
    let _ = writeln!(
        out,
        "  HCOM_DIR={}",
        std::env::var("HCOM_DIR").unwrap_or_else(|_| "<unset>".into())
    );
    let tool_env_var = tool.spec().launch.config_dir_env;
    if let Some(env_var) = tool_env_var {
        let _ = writeln!(
            out,
            "  {env_var}={}",
            std::env::var(env_var).unwrap_or_else(|_| "<unset>".into())
        );
    }
    let _ = writeln!(
        out,
        "  cwd={}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    );
    out
}

/// Verify hooks are installed for the target tool, auto-install if needed.
///
/// Uses verify-first pattern: read-only check first, only write if needed.
/// Strict gate: refuses to launch if hooks can't be installed.
fn ensure_hooks_installed(tool: &LaunchTool, include_permissions: bool) -> Result<()> {
    match tool {
        LaunchTool::Claude | LaunchTool::ClaudePty => {
            if crate::hooks::claude::verify_claude_hooks_installed(None, include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::claude::try_setup_claude_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "settings_path",
                        crate::hooks::claude::get_claude_settings_path(),
                    )],
                );
                bail!(
                    "Failed to setup Claude hooks: {e}\n\
                     Run: hcom hooks add claude\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Gemini => {
            if !crate::hooks::gemini::is_gemini_version_supported() {
                if let Some(ver) = crate::hooks::gemini::get_gemini_version() {
                    bail!(
                        "Gemini CLI version {}.{}.{} is too old. Update: npm i -g @google/gemini-cli@latest",
                        ver.0,
                        ver.1,
                        ver.2
                    );
                } else {
                    eprintln!("Warning: Could not detect Gemini CLI version");
                }
            }
            if crate::hooks::gemini::verify_gemini_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::gemini::try_setup_gemini_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "settings_path",
                        crate::hooks::gemini::get_gemini_settings_path(),
                    )],
                );
                bail!(
                    "Failed to setup Gemini hooks: {e}\n\
                     Run: hcom hooks add gemini\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Codex => {
            if crate::hooks::codex::verify_codex_hooks_installed(include_permissions)
                && crate::hooks::codex::codex_current_feature_enabled()
            {
                return Ok(());
            }
            if let Err(e) = crate::hooks::codex::try_setup_codex_hooks(include_permissions) {
                if matches!(e, crate::hooks::codex::SetupError::HookTrustFailed { .. }) {
                    crate::log::log_warn(
                        "codex",
                        "codex.hook_trust_setup_warn",
                        &format!(
                            "Codex hook setup could not write trust state; launch preprocessing may fall back to hook-trust bypass: {e}"
                        ),
                    );
                } else {
                    let diag = install_diag_context(
                        tool,
                        &[
                            ("config_path", crate::hooks::codex::get_codex_config_path()),
                            ("hooks_path", crate::hooks::codex::get_codex_hooks_path()),
                        ],
                    );
                    bail!(
                        "Failed to setup Codex hooks: {e}\n\
                         Run: hcom hooks add codex\n\
                         {diag}"
                    );
                }
            }
            Ok(())
        }
        LaunchTool::OpenCode => {
            if crate::hooks::opencode::ensure_plugin_installed("opencode") {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup OpenCode plugin. Run: hcom hooks add opencode\n{diag}");
        }
        LaunchTool::Kilo => {
            if crate::hooks::opencode::ensure_plugin_installed("kilo") {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Kilo Code plugin. Run: hcom hooks add kilo\n{diag}");
        }
        LaunchTool::Pi => {
            if crate::hooks::pi::ensure_pi_plugin_installed() {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Pi plugin. Run: hcom hooks add pi\n{diag}");
        }
        LaunchTool::Omp => {
            if crate::hooks::omp::ensure_omp_plugin_installed() {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Oh My Pi plugin. Run: hcom hooks add omp\n{diag}");
        }
        LaunchTool::Antigravity => {
            if crate::hooks::antigravity::verify_antigravity_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) =
                crate::hooks::antigravity::try_setup_antigravity_hooks(include_permissions)
            {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "hooks_path",
                        crate::hooks::antigravity::get_antigravity_hooks_path(),
                    )],
                );
                bail!(
                    "Failed to setup Antigravity hooks: {e}\n\
                     Run: hcom hooks add antigravity\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Cursor => {
            if crate::hooks::cursor::verify_cursor_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::cursor::try_setup_cursor_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[("hooks_path", crate::hooks::cursor::get_cursor_hooks_path())],
                );
                bail!(
                    "Failed to setup Cursor hooks: {e}\n\
                     Run: hcom hooks add cursor\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Kimi => {
            if crate::hooks::kimi::verify_kimi_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::kimi::try_setup_kimi_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[("hooks_path", crate::hooks::kimi::get_kimi_settings_path())],
                );
                bail!(
                    "Failed to setup Kimi hooks: {e}\n\
                     Run: hcom hooks add kimi\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Copilot => {
            if crate::hooks::copilot::verify_copilot_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::copilot::try_setup_copilot_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "hooks_path",
                        crate::hooks::copilot::get_copilot_hooks_path(),
                    )],
                );
                bail!(
                    "Failed to setup Copilot hooks: {e}\n\
                     Run: hcom hooks add copilot\n\
                     {diag}"
                );
            }
            Ok(())
        }
    }
}

/// Build a command string for Claude (non-PTY mode). Quoting matches the
/// shell that will run the resulting script: PowerShell on Windows, POSIX
/// shell elsewhere (see `launch_terminal`'s `create_powershell_script` /
/// `create_bash_script` split).
fn build_claude_command(args: &[String]) -> String {
    let mut parts = vec!["claude".to_string()];
    for arg in args {
        if cfg!(windows) {
            parts.push(terminal::ps_quote(arg));
        } else {
            parts.push(crate::tools::args_common::shell_quote(arg));
        }
    }
    parts.join(" ")
}

/// Tool-specific extra environment variables for PTY mode.
fn tool_extra_env(tool: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    // Claude is driven by the PTY wrapper (ConPTY on Windows, openpty on Unix),
    // which handles injection; HCOM_PTY_MODE tells the Stop hook to defer to it.
    if tool == "claude" {
        m.insert("HCOM_PTY_MODE".to_string(), "1".to_string());
    }
    if tool == "antigravity" {
        m.insert("ANTIGRAVITY_AGENT".to_string(), "1".to_string());
    }
    m
}

pub(super) fn background_runner_env(
    tool: &str,
    env: &HashMap<String, String>,
    instance_name: &str,
) -> HashMap<String, String> {
    let mut runner_env = env.clone();
    runner_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.to_string());
    // Default HCOM_TOOL when the caller didn't already set it (most callers
    // come from `launch()` which inserts it; this keeps the standalone PTY
    // and headless paths consistent so `{tool}` template substitution and
    // delivery-loop label formatting see the right value).
    runner_env
        .entry("HCOM_TOOL".to_string())
        .or_insert_with(|| tool.to_string());
    runner_env.extend(tool_extra_env(tool));
    runner_env
}

/// Non-HCOM ambient env to forward through the sidecar, with marker/identity/
/// instance-state/terminal-color vars stripped. On Windows env names are
/// case-insensitive (and the paired PowerShell `Remove-Item Env:` folds case),
/// so `case_insensitive` folds case for the match; Unix keeps exact-case.
fn sidecar_ambient_env<'a>(
    env: &HashMap<String, String>,
    strip_vars: impl Iterator<Item = &'a str>,
    case_insensitive: bool,
) -> HashMap<String, String> {
    let norm = |k: &str| {
        if case_insensitive {
            k.to_ascii_lowercase()
        } else {
            k.to_string()
        }
    };
    let strip: std::collections::HashSet<String> = strip_vars.map(&norm).collect();
    env.iter()
        .filter(|(k, _)| !k.starts_with("HCOM_") && !strip.contains(&norm(k)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Windows runner: a PowerShell script that launches the tool through the hcom
/// ConPTY wrapper (`hcom pty <tool>`), mirroring the Unix bash runner. The
/// wrapper runs the delivery loop so idle agents can be woken. Mirrors the bash
/// runner's env scrubbing, HCOM env, secret sidecar, and PATH setup.
fn create_runner_script_windows(
    tool: &str,
    cwd: &str,
    instance_name: &str,
    env: &HashMap<String, String>,
    tool_args: &[String],
) -> Result<String> {
    let tool_spec = tool.parse::<crate::tool::Tool>().map(|t| t.spec()).ok();
    let instance_state_env: &[&str] = tool_spec.map(|s| s.instance_state_env).unwrap_or(&[]);

    let launch_dir = paths::hcom_path(&[paths::LAUNCH_DIR]);
    fs::create_dir_all(&launch_dir).ok();
    let script_file = launch_dir.join(format!(
        "{}_{}_{}_{}.ps1",
        tool,
        instance_name,
        std::process::id(),
        rand::random::<u16>() % 9000 + 1000
    ));

    // Visible HCOM_* env, plus the managed-launch marker so hooks engage.
    let mut hcom_env: HashMap<String, String> = env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    hcom_env.insert("HCOM_LAUNCHED".to_string(), "1".to_string());
    let env_block = terminal::build_env_string(&hcom_env, "powershell");

    // Non-HCOM ambient env (may carry secrets) goes through a private sidecar
    // that is dot-sourced then deleted, matching the bash runner.
    let ambient_env = sidecar_ambient_env(
        env,
        tool_marker_vars()
            .iter()
            .chain(HCOM_IDENTITY_VARS.iter())
            .chain(instance_state_env.iter())
            .chain(crate::terminal::TERMINAL_COLOR_VARS.iter())
            .copied(),
        true,
    );
    let sidecar_source = if ambient_env.is_empty() {
        String::new()
    } else {
        let env_file = launch_dir.join(format!(
            "{}_{}_{}_{}.ps1",
            tool,
            instance_name,
            std::process::id(),
            rand::random::<u16>() % 9000 + 1000
        ));
        let mut file = crate::sys::fs::create_private_new(&env_file)?;
        // Windows PowerShell 5.1 reads BOM-less files using the legacy ANSI
        // code page, corrupting non-ASCII env values; a UTF-8 BOM forces it
        // to read as UTF-8.
        file.write_all(b"\xEF\xBB\xBF")?;
        writeln!(
            file,
            "{}",
            terminal::build_env_string(&ambient_env, "powershell")
        )?;
        let q = terminal::ps_quote(&env_file.to_string_lossy());
        format!("if (Test-Path {q}) {{ . {q}; Remove-Item -Force {q} }}")
    };

    // Scrub inherited tool markers / identity / instance-state vars.
    let unset_names: Vec<String> = tool_marker_vars()
        .iter()
        .chain(HCOM_IDENTITY_VARS.iter())
        .chain(instance_state_env.iter())
        .map(|v| format!("Env:{v}"))
        .collect();
    let unset_line = if unset_names.is_empty() {
        String::new()
    } else {
        format!(
            "Remove-Item {} -ErrorAction SilentlyContinue",
            unset_names.join(",")
        )
    };

    // Resolve binary directories for minimal PATH environments.
    let mut path_dirs: Vec<String> = Vec::new();
    if let Ok(dev_root) = std::env::var("HCOM_DEV_ROOT")
        && let Some(bin) = crate::shared::dev_root_binary(Path::new(&dev_root))
        && let Some(dir) = bin.parent()
    {
        path_dirs.push(dir.to_string_lossy().into_owned());
    }
    // Ensure the launched tool (and its hooks) can call back to *this* hcom by
    // name — a dev binary may not be on the global PATH.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let d = dir.to_string_lossy().to_string();
        if !path_dirs.contains(&d) {
            path_dirs.push(d);
        }
    }
    let tool_bin = tool
        .parse::<crate::tool::Tool>()
        .map(|t| t.spec().cli_binary)
        .unwrap_or(tool);
    for bin_name in &[tool_bin, "hcom", "python3", "node"] {
        if let Some(bin_path) = terminal::which_bin(bin_name)
            && let Some(dir) = Path::new(&bin_path).parent()
        {
            let d = dir.to_string_lossy().to_string();
            if !path_dirs.contains(&d) {
                path_dirs.push(d);
            }
        }
    }
    let path_line = if path_dirs.is_empty() {
        String::new()
    } else {
        format!(
            "$env:PATH = {} + $env:PATH",
            terminal::ps_quote(&format!("{};", path_dirs.join(";")))
        )
    };

    // Run through the hcom PTY wrapper (ConPTY) so the tool is driven by the
    // delivery loop — this is what wakes an idle agent on Windows. Mirrors the
    // Unix runner's `hcom pty <tool>` call.
    //
    // Tool args travel via a JSON sidecar file, not the command line: the
    // PowerShell → native-exe argv boundary mangles embedded double quotes
    // (powershell.exe passes them unescaped, so the child's command-line
    // parser re-splits at quote/space boundaries). Codex args always contain
    // quotes (`-c projects={ "path" = ... }`, developer_instructions), which
    // made `hcom codex` fail with "unexpected argument" (#66). Only the
    // sidecar path — generated by hcom, never quote-bearing — goes on the
    // command line. `hcom pty` reads and deletes the file.
    let hcom_bin = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "hcom".to_string());
    let run_line = if tool_args.is_empty() {
        format!("& {} pty {}", terminal::ps_quote(&hcom_bin), tool)
    } else {
        let args_file = launch_dir.join(format!(
            "{}_{}_{}_{}.args.json",
            tool,
            instance_name,
            std::process::id(),
            rand::random::<u16>() % 9000 + 1000
        ));
        let mut file = crate::sys::fs::create_private_new(&args_file)?;
        file.write_all(serde_json::to_string(tool_args)?.as_bytes())?;
        format!(
            "& {} pty {} --hcom-args-file {}",
            terminal::ps_quote(&hcom_bin),
            tool,
            terminal::ps_quote(&args_file.to_string_lossy())
        )
    };
    // `powershell -File` returns 0 unless the script exits with an explicit
    // code, so surface the wrapper's real exit status (agent failures, PTY
    // crashes, kill signals) instead of always reporting success.
    let run_line = format!("{run_line}\nexit $LASTEXITCODE");

    let display = tool
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .collect::<String>()
        + &tool[1..];
    let content = format!(
        "# {display} hcom native runner ({instance_name})\n\
         Set-Location {cwd}\n\
         {unset_line}\n\
         {env_block}\n\
         {sidecar_source}\n\
         {path_line}\n\
         \n\
         {run_line}\n",
        cwd = terminal::ps_quote(cwd),
    );

    // Windows PowerShell 5.1 reads BOM-less files using the legacy ANSI code
    // page, corrupting non-ASCII usernames/paths/prompts/env values; a UTF-8
    // BOM forces it to read as UTF-8.
    let mut bytes = Vec::with_capacity(content.len() + 3);
    bytes.extend_from_slice(b"\xEF\xBB\xBF");
    bytes.extend_from_slice(content.as_bytes());
    fs::write(&script_file, &bytes)?;

    crate::log::log_info(
        "pty",
        "native.script",
        &format!(
            "script={} tool={} instance={} (windows ConPTY launch)",
            script_file.display(),
            tool,
            instance_name
        ),
    );

    Ok(script_file.to_string_lossy().to_string())
}

/// Create a bash script that runs a tool via the hcom native PTY wrapper.
///
/// The script sets up the environment and calls `hcom pty <tool> [args...]`.
pub fn create_runner_script(
    tool: &str,
    cwd: &str,
    instance_name: &str,
    env: &HashMap<String, String>,
    tool_args: &[String],
    run_here: bool,
) -> Result<String> {
    if cfg!(windows) {
        return create_runner_script_windows(tool, cwd, instance_name, env, tool_args);
    }
    // Resolve the tool's IntegrationSpec for instance-state env stripping
    let tool_spec = tool.parse::<crate::tool::Tool>().map(|t| t.spec()).ok();
    let instance_state_env: &[&str] = tool_spec.map(|s| s.instance_state_env).unwrap_or(&[]);
    let native_bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("hcom"));
    let native_bin_str = native_bin.to_string_lossy();

    let launch_dir = paths::hcom_path(&[paths::LAUNCH_DIR]);
    fs::create_dir_all(&launch_dir).ok();

    let script_file = launch_dir.join(format!(
        "{}_{}_{}_{}.sh",
        tool,
        instance_name,
        std::process::id(),
        rand::random::<u16>() % 9000 + 1000
    ));

    // Route ALL forwarded non-HCOM env vars through the 0600 sidecar.
    // The visible .sh only exports HCOM_* vars + PATH + cwd (minimal).
    // This avoids the sensitivity-classification heuristic entirely — no
    // secret ever lands in the 0755 world-readable script.
    let hcom_env: HashMap<String, String> = env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let ambient_env = sidecar_ambient_env(
        env,
        tool_marker_vars()
            .iter()
            .chain(HCOM_IDENTITY_VARS.iter())
            .chain(instance_state_env.iter())
            .chain(crate::terminal::TERMINAL_COLOR_VARS.iter())
            .copied(),
        false,
    );
    let env_block = terminal::build_env_string(&hcom_env, "bash_export");
    let sensitive_env_source = if ambient_env.is_empty() {
        String::new()
    } else {
        let env_file = launch_dir.join(format!(
            "{}_{}_{}_{}.env",
            tool,
            instance_name,
            std::process::id(),
            rand::random::<u16>() % 9000 + 1000
        ));
        let mut file = crate::sys::fs::create_private_new(&env_file)?;
        writeln!(
            file,
            "{}",
            terminal::build_env_string(&ambient_env, "bash_export")
        )?;
        let quoted = crate::tools::args_common::shell_quote(&env_file.to_string_lossy());
        format!("if [ -f {quoted} ]; then\n  . {quoted}\n  rm -f {quoted}\nfi")
    };
    let tool_args_str: String = tool_args
        .iter()
        .map(|a| crate::tools::args_common::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    // Resolve binary paths for minimal PATH environments
    let mut path_dirs: Vec<String> = Vec::new();

    // Dev mode: prepend the worktree's Cargo output dir
    if let Ok(dev_root) = std::env::var("HCOM_DEV_ROOT")
        && let Some(bin) = crate::shared::dev_root_binary(Path::new(&dev_root))
        && let Some(dir) = bin.parent()
    {
        path_dirs.push(dir.to_string_lossy().into_owned());
    }

    let tool_bin = tool
        .parse::<crate::tool::Tool>()
        .map(|t| t.spec().cli_binary)
        .unwrap_or(tool);
    for bin_name in &[tool_bin, "hcom", "python3", "node"] {
        if let Some(bin_path) = terminal::which_bin(bin_name)
            && let Some(dir) = Path::new(&bin_path).parent()
        {
            let d = dir.to_string_lossy().to_string();
            if !path_dirs.contains(&d) {
                path_dirs.push(d);
            }
        }
    }

    let path_export = if !path_dirs.is_empty() {
        format!("export PATH=\"{}:$PATH\"", path_dirs.join(":"))
    } else {
        String::new()
    };

    let use_exec = if run_here { "" } else { "exec " };

    let content = format!(
        "#!/bin/bash\n\
         # {} hcom native PTY runner ({})\n\
         # Using: {}\n\
         cd {}\n\
         \n\
         unset {}\n\
         unset {}\n\
         unset {}\n\
         {}\n\
         {}\n\
         {}\n\
         \n\
         {}{} pty {} {}\n",
        tool.chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .collect::<String>()
            + &tool[1..],
        instance_name,
        native_bin_str,
        crate::tools::args_common::shell_quote(cwd),
        tool_marker_vars().join(" "),
        HCOM_IDENTITY_VARS.join(" "),
        instance_state_env.join(" "),
        env_block,
        sensitive_env_source,
        path_export,
        use_exec,
        crate::tools::args_common::shell_quote(&native_bin_str),
        tool,
        tool_args_str,
    );

    fs::write(&script_file, &content)?;
    crate::sys::fs::set_executable(&script_file)?;

    crate::log::log_info(
        "pty",
        "native.script",
        &format!(
            "script={} tool={} instance={} forwarded_keys=[{}]",
            script_file.display(),
            tool,
            instance_name,
            ambient_env.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    );

    Ok(script_file.to_string_lossy().to_string())
}

/// Build the command that runs a generated runner script in the launched
/// terminal: PowerShell on Windows, bash elsewhere.
fn runner_invocation_command(script_file: &str) -> String {
    if cfg!(windows) {
        format!(
            "powershell -ExecutionPolicy Bypass -File {}",
            crate::terminal::ps_quote(script_file)
        )
    } else {
        format!(
            "bash {}",
            crate::tools::args_common::shell_quote(script_file)
        )
    }
}

/// Launch a tool via PTY wrapper in a terminal.
#[allow(clippy::too_many_arguments)]
pub fn launch_pty(
    tool: &str,
    cwd: &str,
    env: &HashMap<String, String>,
    instance_name: &str,
    tool_args: &[String],
    run_here: bool,
    terminal: Option<&str>,
    inside_ai_tool: bool,
) -> Result<bool> {
    if env.get("HCOM_PROCESS_ID").is_none_or(|v| v.is_empty()) {
        crate::log::log_error(
            "pty",
            "pty.exit",
            &format!("HCOM_PROCESS_ID not set in env for {}", instance_name),
        );
        return Ok(false);
    }

    let mut runner_env = env.clone();
    runner_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.to_string());
    runner_env
        .entry("HCOM_TOOL".to_string())
        .or_insert_with(|| tool.to_string());
    runner_env.extend(tool_extra_env(tool));

    let script_file =
        create_runner_script(tool, cwd, instance_name, &runner_env, tool_args, run_here)?;

    let command = runner_invocation_command(&script_file);
    let terminal_env: HashMap<String, String> = runner_env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let (launch_result, effective_preset) = terminal::launch_terminal(
        &command,
        &terminal_env,
        Some(cwd),
        false, // not background
        run_here,
        terminal,
        inside_ai_tool,
    )?;

    instance_binding::persist_terminal_launch_context(
        &crate::db::HcomDb::open()?,
        instance_name,
        terminal,
        &effective_preset,
        env.get("HCOM_PROCESS_ID").map(|s| s.as_str()),
    );

    match launch_result {
        terminal::LaunchResult::Success => Ok(true),
        terminal::LaunchResult::Background(_, _) => Ok(true),
        terminal::LaunchResult::Failed(_) => Ok(false),
    }
}

/// Identity and tracking context for a background launch, shared across tool types.
struct BackgroundLaunchCtx<'a> {
    db: &'a HcomDb,
    tool: &'a str,
    instance_name: &'a str,
    process_id: &'a str,
    terminal_mode: Option<&'a str>,
    tag: &'a str,
    working_dir: &'a str,
    log_files: &'a mut Vec<String>,
    handles: &'a mut Vec<serde_json::Value>,
}

/// Shared bookkeeping after a successful background launch for gemini/codex/opencode.
/// Persists the launch context, updates position, records the PID, and appends
/// log_file / handle entries. Per-tool differences (args, prompt) stay in the caller.
fn finalize_background_launch(
    ctx: &mut BackgroundLaunchCtx<'_>,
    log_file: String,
    pid: u32,
    effective_preset: String,
) {
    instance_binding::persist_terminal_launch_context(
        ctx.db,
        ctx.instance_name,
        ctx.terminal_mode,
        &effective_preset,
        Some(ctx.process_id),
    );
    instances::update_instance_position(
        ctx.db,
        ctx.instance_name,
        &serde_json::Map::from_iter([
            ("pid".to_string(), json!(pid)),
            ("background_log_file".to_string(), json!(&log_file)),
        ]),
    );
    crate::pidtrack::record_pid(&crate::pidtrack::PidRecord {
        process_id: ctx.process_id,
        terminal_preset: &effective_preset,
        tag: ctx.tag,
        ..crate::pidtrack::PidRecord::new(
            &crate::paths::hcom_dir(),
            pid,
            ctx.tool,
            ctx.instance_name,
            ctx.working_dir,
        )
    });
    ctx.log_files.push(log_file.clone());
    ctx.handles.push(json!({
        "tool": ctx.tool,
        "instance_name": ctx.instance_name,
        "log_file": log_file,
        "pid": pid,
    }));
}

fn launch_background_runner(
    tool: &str,
    cwd: &str,
    instance_name: &str,
    instance_env: &mut HashMap<String, String>,
    tool_args: &[String],
    terminal_mode: Option<&str>,
    inside_ai_tool: bool,
) -> Result<(String, u32, String)> {
    let log_filename = format!(
        "background_{}_{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        rand::random::<u16>() % 9000 + 1000
    );
    let mut runner_env = background_runner_env(tool, instance_env, instance_name);
    runner_env.insert("HCOM_BACKGROUND".to_string(), log_filename);
    let script_file =
        create_runner_script(tool, cwd, instance_name, &runner_env, tool_args, false)?;
    let command = runner_invocation_command(&script_file);
    let terminal_env: HashMap<String, String> = runner_env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let (launch_result, effective_preset) = terminal::launch_terminal(
        &command,
        &terminal_env,
        Some(cwd),
        true,
        false,
        terminal_mode,
        inside_ai_tool,
    )?;
    match launch_result {
        terminal::LaunchResult::Background(log_file, pid) => Ok((log_file, pid, effective_preset)),
        _ => bail!("background launch failed"),
    }
}

/// Common launch path for gemini/codex/opencode: background or PTY foreground.
///
/// Handles `launch_background_runner` + `finalize_background_launch` for background,
/// and `will_run_in_current_terminal` + `launch_pty` for foreground.
fn launch_pty_or_background(
    ctx: &mut BackgroundLaunchCtx<'_>,
    instance_env: &mut HashMap<String, String>,
    tool_args: &[String],
    params: &LaunchParams,
    inside_ai_tool: bool,
) -> Result<bool> {
    if params.background {
        let (log_file, pid, effective_preset) = launch_background_runner(
            ctx.tool,
            ctx.working_dir,
            ctx.instance_name,
            instance_env,
            tool_args,
            ctx.terminal_mode,
            inside_ai_tool,
        )?;
        finalize_background_launch(ctx, log_file, pid, effective_preset);
        Ok(true)
    } else {
        let effective_run_here = will_run_in_current_terminal(
            params.count,
            false,
            params.run_here,
            ctx.terminal_mode,
            inside_ai_tool,
        );
        let ok = launch_pty(
            ctx.tool,
            ctx.working_dir,
            instance_env,
            ctx.instance_name,
            tool_args,
            effective_run_here,
            ctx.terminal_mode,
            inside_ai_tool,
        )?;
        if ok {
            ctx.handles
                .push(json!({"tool": ctx.tool, "instance_name": ctx.instance_name}));
        }
        Ok(ok)
    }
}

/// Resolve a naming conflict for an explicit instance name.
///
/// - Name is free → Ok(()).
/// - Name held by an inactive row → consume the row (delete) and return Ok(()).
///   An inactive row is a resume handle from agy soft-finalize; launch will
///   re-create a fresh row with the same name.
/// - Name held by a `pending` placeholder reservation → Ok(()) without
///   deleting. This is *our own* reservation: the fork/resume path calls
///   `reserve_generated_name` (under flock, against an unused name) before the
///   launch, then passes that name as `params.name`. The pre-register step
///   (`initialize_instance_in_position_file`) promotes the placeholder in
///   place, so it must survive — bailing here broke every tracked `hcom f`.
/// - Name held by anything else (listening/active/blocked) → Err.
fn resolve_explicit_name_conflict(db: &HcomDb, name: &str) -> Result<()> {
    let Some(row) = db.get_instance(name).ok().flatten() else {
        return Ok(());
    };
    let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "inactive" {
        db.delete_instance(name).map_err(|e| {
            anyhow::anyhow!("Failed to clear inactive resume row '{}': {}", name, e)
        })?;
        return Ok(());
    }
    // A pending placeholder with no session yet is a reservation, not a live
    // agent — leave it for the launcher's pre-register promotion.
    let status_context = row
        .get("status_context")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_empty = row
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(true);
    if status == instance_names::PLACEHOLDER_STATUS
        && status_context == instance_names::PLACEHOLDER_CONTEXT
        && session_empty
    {
        return Ok(());
    }
    bail!(
        "Instance '{}' already exists (stop it first or use a different name)",
        name
    );
}

/// Launch one or more AI tool instances with consistent tracking.
///
/// This is the unified entry point for launching Claude, Gemini, Codex,
/// and OpenCode instances with batch tracking, environment setup, and
/// error handling.
pub fn launch(db: &HcomDb, mut params: LaunchParams) -> Result<LaunchResult> {
    // Claude background defaults to the live PTY surface (`ClaudePty`); it only
    // drops to the `-p`/`--print` surface (`Claude`/`NativePrint`) when the
    // caller explicitly passes `-p`/`--print` in the args. Both stay alive —
    // PTY hosts the live TUI, print mode loops via the stop hook. `-p` is gated
    // behind explicit opt-in because, from 2026-06-15, `claude -p` draws from a
    // separate Agent SDK credit pool on subscription plans.
    // Exact-token match is intentional: `--print` is a boolean flag, so an
    // equals form (`--print=x`) is malformed for claude and surfaces as a
    // launch failure rather than being silently mis-routed to the PTY surface.
    let claude_print = (params.tool == "claude" || params.tool == "claude-pty")
        && params
            .args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-p" | "--print"));

    // `claude-pty` is the PTY surface; `-p`/`--print` selects the NativePrint
    // surface. The two are mutually exclusive: passing both would route print
    // mode through the HeadlessPty runner, which is broken. Fail fast.
    if params.tool == "claude-pty" && claude_print {
        bail!(
            "The PTY surface does not support `-p`/`--print`.\n\
             Use one of:\n\
             • `hcom claude --headless`  — live PTY session\n\
             • `hcom claude -p ...`      — print/pipe mode"
        );
    }

    let normalized = if params.tool == "claude" && !claude_print {
        LaunchTool::ClaudePty
    } else {
        LaunchTool::from_str(&params.tool)?
    };
    let base_tool = normalized.base_tool();
    let backend = LaunchBackend::resolve(&normalized, params.background);

    // Validation
    validate_launch_count(&normalized, params.count)?;

    // HCOM_DIR placement: refuse if it sits under a tool-protected metadata
    // directory. codex hard-denies apply_patch into these via
    // FileSystemSandboxPolicy with no approval path; claude/gemini gate them
    // behind permission prompts on every hcom write. Either way the user gets
    // a broken session — fail fast at launch with a clear message instead.
    let hcom_dir_path = paths::hcom_dir();
    if let Some(protected) = paths::protected_hcom_dir_component(&hcom_dir_path) {
        bail!(
            "HCOM_DIR ({}) sits under a protected directory component '{}'.\n\
             AI tools (codex/claude/gemini) deny writes under .git/.codex/.claude/.agents,\n\
             which would block hcom DB writes from the launched agent.\n\
             Set HCOM_DIR to a path outside these directories.",
            hcom_dir_path.display(),
            protected
        );
    }

    let tool_binary = normalized.cli_binary();
    if !is_tool_installed(tool_binary) {
        bail!("'{}' is not installed or not in PATH", tool_binary);
    }

    if !params.skip_validation {
        let validation_errors = validate_tool_args(&normalized, &params.args);
        if !validation_errors.is_empty() {
            bail!("{}", validation_errors.join("\n"));
        }
    }

    // Load config before hook setup so auto_approve is authoritative for
    // wrapped launches as well as manual `hcom hooks add`.
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|e| {
        eprintln!("[hcom] warn: config load failed, using defaults: {e}");
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    // For Codex: probe CODEX_HOME writability synchronously. Sandboxed parent
    // codex would otherwise spawn a child that hangs on the readonly-state-DB
    // repair prompt. Failing here lets the parent's sandbox-escalation flow
    // surface the denial to the user.
    if matches!(normalized, LaunchTool::Codex) {
        crate::tools::codex_preprocessing::ensure_codex_home_writable()?;
    }

    let inside_ai_tool = crate::shared::context::HcomContext::from_os().is_inside_ai_tool();
    let terminal_mode = params
        .terminal
        .as_deref()
        .or(Some(hcom_config.terminal.as_str()).filter(|t| !t.is_empty()));
    let base_env_run_here = will_run_in_current_terminal(
        params.count,
        params.background,
        params.run_here,
        terminal_mode,
        inside_ai_tool,
    );

    // Ensure hooks are installed (strict: refuse to launch without hooks)
    ensure_hooks_installed(&normalized, hcom_config.auto_approve)?;

    // Build base environment for the current launch regime, then overlay
    // config.toml + ~/.hcom/env which win.
    let mut base_env = build_launch_env(
        &hcom_config,
        launch_env_regime(base_env_run_here, inside_ai_tool),
    );
    if let Some(ref caller_env) = params.env {
        base_env.extend(caller_env.clone());
    }
    base_env.remove("HCOM_TERMINAL");
    if let Some(env_var) = normalized.spec().launch.config_dir_env
        && !base_env.contains_key(env_var)
        && std::env::var(env_var)
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
        && let Some(config_dir) = isolated_tool_config_dir(&normalized)
    {
        base_env.insert(
            env_var.to_string(),
            config_dir.to_string_lossy().to_string(),
        );
    }

    // Tag resolution
    let effective_tag = if let Some(ref tag) = params.tag {
        base_env.insert("HCOM_TAG".to_string(), tag.clone());
        tag.clone()
    } else if let Some(tag) = base_env.get("HCOM_TAG").cloned() {
        tag
    } else {
        let default = hcom_config.tag.clone();
        if !default.is_empty() {
            base_env.insert("HCOM_TAG".to_string(), default.clone());
        }
        default
    };

    // Explicit name validation
    if let Some(ref name) = params.name {
        if params.count > 1 {
            bail!(
                "Cannot use explicit name with count > 1 (count={})",
                params.count
            );
        }
        resolve_explicit_name_conflict(db, name)?;
    }

    // System prompt file for Gemini/Codex
    if let Some(ref sp) = params.system_prompt
        && normalized == LaunchTool::Gemini
    {
        let path = write_system_prompt_file(sp, "gemini");
        base_env.insert("GEMINI_SYSTEM_MD".to_string(), path);
    }

    let working_dir = params.cwd.as_deref().unwrap_or(".");
    let canonical_dir = std::fs::canonicalize(working_dir)
        .unwrap_or_else(|_| std::path::PathBuf::from(working_dir));
    // Folder trust: on first run each tool shows a "do you trust this folder?"
    // prompt — the user accepts to continue or declines and it exits. When an
    // agent launches another agent via hcom, auto-approve the prompt for the
    // launch dir to smooth the process. Cursor's lever is a marker file (its
    // `--trust` flag is print-only, inert in our PTY), so it's seeded here;
    // gemini/codex use arg injection below.
    if hcom_config.auto_trust_workspace && matches!(normalized, LaunchTool::Cursor) {
        cursor_preprocessing::ensure_cursor_workspace_trusted(&canonical_dir)?;
    }
    if hcom_config.auto_trust_workspace && matches!(normalized, LaunchTool::Copilot) {
        copilot_preprocessing::ensure_copilot_workspace_trusted(&canonical_dir)?;
    }

    // Scrub any hcom-managed OMP extension arg a previous hcom version may have
    // baked into the stored args, from BOTH the live and the persisted vectors
    // (the snapshot below prefers persisted_args, and resume supplies that
    // historical vector separately). Without this, replaying an old session can
    // pass a stale `-e <old hcom.ts>` alongside the freshly injected current
    // path — failing startup or loading hcom twice. User extensions are kept.
    if matches!(normalized, LaunchTool::Omp) {
        crate::hooks::omp::strip_managed_extension_args(&mut params.args);
        if let Some(persisted) = params.persisted_args.as_mut() {
            crate::hooks::omp::strip_managed_extension_args(persisted);
        }
    }

    // Capture the persistable args BEFORE any hcom launch injection below.
    // Resume replays only user/config args; workspace-trust injection
    // (gemini `--skip-trust`, codex `-c projects=…trust_level`), the OMP
    // delivery-extension path (`-e <abs hcom.ts>`), and the `--hcom-prompt`
    // translation are session/path-specific and must not be baked into
    // launch_args, or they would replay stale state on resume/fork (e.g. a
    // stale plugin path if PI_CODING_AGENT_DIR or the install location moves).
    let stored_launch_args = params
        .persisted_args
        .clone()
        .unwrap_or_else(|| params.args.clone());

    // Injected after the snapshot so the internal plugin path is never persisted;
    // resume re-injects the current path via the same call.
    inject_omp_extension_args(&normalized, &mut params.args);

    inject_workspace_trust_args(
        &normalized,
        &canonical_dir,
        &mut params.args,
        hcom_config.auto_trust_workspace,
    );
    let launcher_name: String = params.launcher.take().unwrap_or_else(|| {
        // Try to resolve caller identity from the live process binding.
        let process_id = std::env::var("HCOM_PROCESS_ID").ok();
        match crate::identity::resolve_identity(
            db,
            None,
            None,
            None,
            process_id.as_deref(),
            None,
            None,
        ) {
            Ok(id) => id.name,
            Err(_) => "api".to_string(),
        }
    });

    // Inject --hcom-prompt into tool args (translated per-tool).
    // When a real hcom participant launched us, append a reply instruction so
    // the spawned agent knows to send its result back.
    if let Some(ref prompt) = params.initial_prompt {
        let reply_suffix =
            if params.append_reply_handoff && launcher_name != "api" && launcher_name != "user" {
                format!("\n\nWhen done, send your result back to @{launcher_name} via hcom.")
            } else {
                String::new()
            };
        let full_prompt = format!("{prompt}{reply_suffix}");
        append_initial_prompt_args(&normalized, &mut params.args, full_prompt)?;
    }
    let batch_id = params
        .batch_id
        .take()
        .unwrap_or_else(|| format!("{:08x}", rand::rng().random::<u32>()));

    let mut launched = 0usize;
    let mut log_files: Vec<String> = Vec::new();
    let mut handles: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();

    for _ in 0..params.count {
        let mut instance_env = base_env.clone();
        instance_env.insert("HCOM_LAUNCHED".to_string(), "1".to_string());
        instance_env.insert(
            "HCOM_LAUNCH_EVENT_ID".to_string(),
            db.get_last_event_id().to_string(),
        );
        instance_env.insert("HCOM_LAUNCHED_BY".to_string(), launcher_name.to_string());
        instance_env.insert("HCOM_LAUNCH_BATCH_ID".to_string(), batch_id.clone());
        instance_env.insert(
            "HCOM_DIR".to_string(),
            paths::hcom_dir().to_string_lossy().to_string(),
        );

        // Propagate dev root
        if let Ok(val) = std::env::var("HCOM_DEV_ROOT") {
            instance_env.insert("HCOM_DEV_ROOT".to_string(), val);
        }
        // Propagate HCOM_NOTES
        if let Ok(val) = std::env::var("HCOM_NOTES") {
            instance_env.insert("HCOM_NOTES".to_string(), val);
        }

        let process_id = generate_process_id();
        instance_env.insert("HCOM_PROCESS_ID".to_string(), process_id.clone());

        // Fork mode detection
        if matches!(normalized, LaunchTool::Claude | LaunchTool::ClaudePty)
            && params.args.iter().any(|a| a == "--fork-session")
        {
            instance_env.insert("HCOM_IS_FORK".to_string(), "1".to_string());
        }

        let instance_name = if let Some(ref name) = params.name {
            name.clone()
        } else {
            instance_names::generate_unique_name(db)?
        };
        instance_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.clone());

        // Process ID export: allow custom env var name
        if let Ok(export_var) = std::env::var("HCOM_PROCESS_ID_EXPORT")
            && !export_var.is_empty()
        {
            instance_env.insert(export_var, process_id.clone());
        }

        // Name/process export vars
        if let Ok(export_var) = std::env::var("HCOM_NAME_EXPORT") {
            if !export_var.is_empty() {
                instance_env.insert(export_var, instance_name.clone());
            }
        } else if !hcom_config.name_export.is_empty() {
            instance_env.insert(hcom_config.name_export.clone(), instance_name.clone());
        }

        let tool_type = base_tool;
        instance_env.insert("HCOM_TOOL".to_string(), tool_type.to_string());

        // Pre-format the pane title for templates that substitute
        // `{pane_title}` (custom user templates only — the built-in herdr
        // preset passes `{instance_name}` and the delivery loop pushes the
        // styled label via `pane.rename`). `display_for_title` mirrors
        // `identity::get_display_name` for the about-to-be-created instance
        // row.
        let display_for_title = if effective_tag.is_empty() {
            instance_name.clone()
        } else {
            format!("{}-{}", effective_tag, instance_name)
        };
        instance_env.insert(
            "HCOM_PANE_TITLE".to_string(),
            crate::shared::format_pane_title(
                crate::shared::ST_LISTENING,
                &display_for_title,
                tool_type,
            ),
        );

        // Pre-register instance
        if let Err(e) = (|| -> Result<()> {
            instance_binding::initialize_instance_in_position_file(
                db,
                &instance_name,
                params.prior_session_id.as_deref(),
                None,            // parent_session_id
                None,            // parent_name
                None,            // agent_id
                None,            // transcript_path
                Some(tool_type), // tool
                params.background,
                if effective_tag.is_empty() {
                    None
                } else {
                    Some(effective_tag.as_str())
                },
                None,              // wait_timeout
                None,              // subagent_timeout
                None,              // hints
                Some(working_dir), // cwd_override: use launch params cwd, not current_dir()
            );
            db.set_process_binding(&process_id, "", &instance_name)?;
            Ok(())
        })() {
            errors.push(json!({"tool": base_tool, "error": e.to_string()}));
            continue;
        }

        // Dispatch to tool-specific launcher
        let launch_result = (|| -> Result<bool> {
            match normalized {
                LaunchTool::Claude => {
                    let claude_cmd = build_claude_command(&params.args);

                    // Store launch_args
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );

                    // LaunchTool::Claude only resolves to NativePrint (background,
                    // direct spawn in print mode) or InteractiveVisible — the
                    // PTY-backed variants live in LaunchTool::ClaudePty below.
                    if matches!(backend, LaunchBackend::NativePrint) {
                        let log_filename = format!(
                            "background_{}_{}.log",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0),
                            rand::random::<u16>() % 9000 + 1000
                        );
                        instance_env.insert("HCOM_BACKGROUND".to_string(), log_filename.clone());

                        let (launch_result, effective_preset) = terminal::launch_terminal(
                            &claude_cmd,
                            &instance_env,
                            Some(working_dir),
                            true, // background
                            false,
                            terminal_mode,
                            inside_ai_tool,
                        )?;
                        match launch_result {
                            terminal::LaunchResult::Background(log_file, pid) => {
                                finalize_background_launch(
                                    &mut BackgroundLaunchCtx {
                                        db,
                                        tool: "claude",
                                        instance_name: &instance_name,
                                        process_id: &process_id,
                                        terminal_mode,
                                        tag: params.tag.as_deref().unwrap_or(""),
                                        working_dir,
                                        log_files: &mut log_files,
                                        handles: &mut handles,
                                    },
                                    log_file,
                                    pid,
                                    effective_preset,
                                );
                                Ok(true)
                            }
                            _ => Ok(false),
                        }
                    } else {
                        let effective_run_here = will_run_in_current_terminal(
                            params.count,
                            false,
                            params.run_here,
                            terminal_mode,
                            inside_ai_tool,
                        );
                        let (launch_result, effective_preset) = terminal::launch_terminal(
                            &claude_cmd,
                            &instance_env,
                            Some(working_dir),
                            false,
                            effective_run_here,
                            terminal_mode,
                            inside_ai_tool,
                        )?;
                        instance_binding::persist_terminal_launch_context(
                            db,
                            &instance_name,
                            terminal_mode,
                            &effective_preset,
                            Some(&process_id),
                        );

                        match launch_result {
                            terminal::LaunchResult::Success => {
                                handles.push(
                                    json!({"tool": "claude", "instance_name": instance_name}),
                                );
                                Ok(true)
                            }
                            _ => Ok(false),
                        }
                    }
                }

                LaunchTool::ClaudePty => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    // Same background/foreground split as gemini/codex/opencode:
                    // foreground → visible PTY in a terminal; background → PTY
                    // wrapper in a detached runner. The wrapper handles the TUI
                    // the same way either way, which is what lets PTY-headless
                    // claude keep a live session that accepts hcom inject.
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "claude",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::Gemini => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "gemini",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::Codex => {
                    // Bootstrap delivered via developer_instructions at launch
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([("name_announced".to_string(), json!(true))]),
                    );

                    // Build effective args: system_prompt + preprocessing
                    let mut effective_args = params.args.clone();
                    if let Some(ref sp) = params.system_prompt {
                        let mut pre =
                            vec!["-c".to_string(), format!("developer_instructions={}", sp)];
                        pre.extend(effective_args);
                        effective_args = pre;
                    }

                    // Generate bootstrap text for preprocessing
                    let bootstrap = crate::bootstrap::get_bootstrap(
                        db,
                        &paths::hcom_dir(),
                        &instance_name,
                        "codex",
                        params.background,
                        true, // is_launched
                        "",
                        &effective_tag,
                        hcom_config.relay_enabled,
                        None,
                    );

                    let sandbox_mode = instance_env
                        .get("HCOM_CODEX_SANDBOX_MODE")
                        .cloned()
                        .unwrap_or_else(|| "workspace".to_string());

                    effective_args = codex_preprocessing::preprocess_codex_args(
                        &effective_args,
                        &bootstrap,
                        &sandbox_mode,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );

                    instance_env.insert("HCOM_CODEX_SANDBOX_MODE".to_string(), sandbox_mode);

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "codex",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &effective_args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::OpenCode | LaunchTool::Kilo | LaunchTool::Pi | LaunchTool::Omp => {
                    opencode_preprocessing::preprocess_opencode_env(
                        &mut instance_env,
                        base_tool,
                        &instance_name,
                        hcom_config.auto_approve,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: base_tool,
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
                LaunchTool::Antigravity => {
                    // Antigravity ignores GEMINI_SYSTEM_MD; bootstrap is delivered via the
                    // SessionStart hook's inject_bootstrap_once (name_announced stays 0 here).
                    instance_env.insert("ANTIGRAVITY_AGENT".to_string(), "1".to_string());

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "antigravity",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
                LaunchTool::Cursor => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "cursor",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::Kimi => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "kimi",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
                LaunchTool::Copilot => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "copilot",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
            }
        })();

        match launch_result {
            Ok(true) => launched += 1,
            Ok(false) => {
                cleanup_instance(db, &instance_name, &process_id);
            }
            Err(e) => {
                cleanup_instance(db, &instance_name, &process_id);
                errors.push(json!({"tool": base_tool, "error": e.to_string()}));
            }
        }
    }

    let failed = params.count - launched;
    if launched == 0 {
        if !errors.is_empty() {
            let details: Vec<String> = errors
                .iter()
                .filter_map(|e| {
                    e.get("error")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            bail!(
                "No instances launched (0/{}): {}",
                params.count,
                details.join("; ")
            );
        }
        bail!("No instances launched (0/{})", params.count);
    }

    // Log batch launch event and push to relay (best-effort)
    batch::log_batch_completion(
        db,
        &launcher_name,
        &batch_id,
        base_tool,
        params.count,
        launched,
        failed,
        params.background,
        &effective_tag,
        &handles,
    );

    Ok(LaunchResult {
        // User-facing identity. The PTY-vs-print backend distinction lives in
        // `LaunchBackend`, not the tool string, so consumers see `claude`.
        tool: base_tool.to_string(),
        batch_id,
        launched,
        failed,
        background: params.background,
        log_files,
        handles,
        errors,
    })
}

/// Clean up instance and process binding on failure.
fn cleanup_instance(db: &HcomDb, name: &str, process_id: &str) {
    db.delete_instance(name).ok();
    db.delete_process_binding(process_id).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_claude_command() {
        let args = vec!["--model".to_string(), "sonnet".to_string()];
        let cmd = build_claude_command(&args);
        if cfg!(windows) {
            // ps_quote() always quotes, unlike the POSIX shell_quote() used
            // elsewhere, which leaves plain args unquoted.
            assert_eq!(cmd, "claude '--model' 'sonnet'");
        } else {
            assert_eq!(cmd, "claude --model sonnet");
        }
    }

    #[test]
    fn test_build_claude_command_with_spaces() {
        let args = vec!["--prompt".to_string(), "fix all tests".to_string()];
        let cmd = build_claude_command(&args);
        assert!(cmd.contains("'fix all tests'"));
    }

    #[test]
    fn test_background_runner_env_includes_instance_name() {
        let mut env = HashMap::new();
        env.insert("HCOM_PROCESS_ID".to_string(), "pid-123".to_string());

        let runner_env = background_runner_env("codex", &env, "nita");

        assert_eq!(
            runner_env.get("HCOM_INSTANCE_NAME").map(String::as_str),
            Some("nita")
        );
        assert_eq!(
            runner_env.get("HCOM_PROCESS_ID").map(String::as_str),
            Some("pid-123")
        );
        assert!(!runner_env.contains_key("HCOM_PTY_MODE"));
    }

    #[test]
    fn test_background_runner_env_includes_claude_pty_mode() {
        let env = HashMap::new();

        let runner_env = background_runner_env("claude", &env, "hone");

        assert_eq!(
            runner_env.get("HCOM_INSTANCE_NAME").map(String::as_str),
            Some("hone")
        );
        assert_eq!(
            runner_env.get("HCOM_PTY_MODE").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn test_background_runner_env_antigravity_sets_agent() {
        let runner_env = background_runner_env("antigravity", &HashMap::new(), "nabe");
        assert_eq!(
            runner_env.get("ANTIGRAVITY_AGENT").map(String::as_str),
            Some("1")
        );
    }

    // Unix-only: asserts the bash runner's `. 'sidecar'` sourcing + unset block;
    // Windows generates a PowerShell runner with a different shape.
    #[cfg(unix)]
    #[test]
    fn test_runner_script_strips_instance_state_vars() {
        let env = HashMap::from([
            ("GEMINI_PTY_INFO".to_string(), "child_process".to_string()),
            ("GEMINI_API_KEY".to_string(), "gem-key".to_string()),
            ("GEMINI_CLI".to_string(), "1".to_string()),
            ("TERM".to_string(), "dumb".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
            ("FORCE_COLOR".to_string(), "1".to_string()),
            ("RORI_MY_VAR".to_string(), "myval".to_string()),
        ]);

        let script = create_runner_script("gemini", "/tmp", "test", &env, &[], false).unwrap();

        let content = std::fs::read_to_string(&script).unwrap();
        // Instance-state stripped from unset block
        assert!(
            content.contains("GEMINI_PTY_INFO"),
            "GEMINI_PTY_INFO should appear in unset"
        );
        let env_file = content
            .lines()
            .find_map(|line| line.trim().strip_prefix(". "))
            .map(|path| path.trim_matches('\'').to_string())
            .expect("runner script should source a sidecar env file");
        let sidecar = std::fs::read_to_string(&env_file).unwrap();
        assert!(!sidecar.contains("GEMINI_PTY_INFO"));
        assert!(!sidecar.contains("GEMINI_CLI"));
        assert!(!sidecar.contains("TERM="));
        assert!(!sidecar.contains("COLORTERM="));
        assert!(!sidecar.contains("NO_COLOR="));
        assert!(!sidecar.contains("FORCE_COLOR="));
        assert!(sidecar.contains("GEMINI_API_KEY"));
        assert!(sidecar.contains("RORI_MY_VAR"));

        std::fs::remove_file(&script).ok();
        std::fs::remove_file(env_file).ok();
    }

    // create_runner_script_windows() isn't cfg(windows)-gated (only its call
    // site is, via a runtime cfg!(windows) check), so this runs on any host.
    #[test]
    fn test_runner_script_windows_has_bom_and_propagates_exit_code() {
        let env = HashMap::from([("SOME_SECRET".to_string(), "sekrit".to_string())]);

        let script = create_runner_script_windows("gemini", "/tmp", "test-win", &env, &[]).unwrap();

        let bytes = std::fs::read(&script).unwrap();
        assert_eq!(
            &bytes[..3],
            b"\xEF\xBB\xBF",
            "PowerShell 5.1 misreads BOM-less files as the legacy ANSI code page"
        );
        let content = String::from_utf8(bytes[3..].to_vec()).unwrap();
        assert!(
            content.contains("exit $LASTEXITCODE"),
            "runner must surface the wrapped process's real exit code, not always report success"
        );

        let sidecar = content
            .split("Test-Path '")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .expect("ambient env should be sourced from a sidecar file");
        let sidecar_bytes = std::fs::read(sidecar).unwrap();
        assert_eq!(
            &sidecar_bytes[..3],
            b"\xEF\xBB\xBF",
            "sidecar env file needs the same BOM as the runner script"
        );
        assert!(String::from_utf8_lossy(&sidecar_bytes).contains("SOME_SECRET"));

        std::fs::remove_file(&script).ok();
        std::fs::remove_file(sidecar).ok();
    }

    // Tool args must travel via the JSON sidecar, never inline on the run
    // line: powershell.exe passes embedded double quotes unescaped to native
    // executables, so the child re-splits argv at quote boundaries (#66 —
    // `hcom codex` failed on its quote-bearing `-c` values).
    #[test]
    fn test_runner_script_windows_passes_args_via_sidecar_file() {
        let env = HashMap::new();
        let args = vec![
            "-c".to_string(),
            r#"projects={ "C:\repo" = { trust_level = "trusted" } }"#.to_string(),
            "-c".to_string(),
            "developer_instructions=multi\nline \"quoted\" text".to_string(),
        ];

        let script =
            create_runner_script_windows("codex", "/tmp", "test-args", &env, &args).unwrap();
        let content = std::fs::read_to_string(&script).unwrap();

        let run_line = content
            .lines()
            .find(|l| l.contains(" pty codex"))
            .expect("runner must invoke hcom pty");
        assert!(run_line.contains("--hcom-args-file"));
        assert!(!run_line.contains("trust_level"));
        assert!(!run_line.contains("developer_instructions"));

        let args_file = run_line
            .split("--hcom-args-file '")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .expect("run line should quote the args file path");
        let json = std::fs::read_to_string(args_file).unwrap();
        let roundtrip: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(
            roundtrip, args,
            "args must survive the file round-trip exactly"
        );

        std::fs::remove_file(&script).ok();
        std::fs::remove_file(args_file).ok();
    }

    #[test]
    fn test_runner_script_windows_no_args_skips_sidecar_file() {
        let env = HashMap::new();
        let script =
            create_runner_script_windows("gemini", "/tmp", "test-noargs", &env, &[]).unwrap();
        let content = std::fs::read_to_string(&script).unwrap();
        let run_line = content
            .lines()
            .find(|l| l.contains(" pty gemini"))
            .expect("runner must invoke hcom pty");
        assert!(!run_line.contains("--hcom-args-file"));
        std::fs::remove_file(&script).ok();
    }

    fn launcher_test_db() -> crate::db::HcomDb {
        let db = crate::db::HcomDb::open_raw(std::path::Path::new(":memory:")).unwrap();
        db.init_db().unwrap();
        db
    }

    fn insert_test_instance(db: &crate::db::HcomDb, name: &str, status: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, created_at, tool) VALUES (?1, ?2, ?3, 'antigravity')",
                rusqlite::params![name, status, now],
            )
            .unwrap();
    }

    #[test]
    fn resolve_explicit_name_conflict_allows_free_name() {
        let db = launcher_test_db();
        assert!(resolve_explicit_name_conflict(&db, "luna").is_ok());
    }

    #[test]
    fn resolve_explicit_name_conflict_consumes_inactive_resume_row() {
        let db = launcher_test_db();
        insert_test_instance(&db, "zeno", "inactive");
        assert!(resolve_explicit_name_conflict(&db, "zeno").is_ok());
        // Row must be gone so the launcher can create a fresh row with the same name.
        assert!(db.get_instance("zeno").unwrap().is_none());
    }

    #[test]
    fn resolve_explicit_name_conflict_allows_pending_placeholder() {
        // A pending placeholder is the fork/resume path's own reservation
        // (reserve_generated_name). It must pass through so the launcher's
        // pre-register step can promote it — bailing here broke `hcom f`.
        let db = launcher_test_db();
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, status_context, created_at, tool) \
                 VALUES (?1, ?2, ?3, ?4, 'claude')",
                rusqlite::params![
                    "milo",
                    instance_names::PLACEHOLDER_STATUS,
                    instance_names::PLACEHOLDER_CONTEXT,
                    now
                ],
            )
            .unwrap();
        assert!(resolve_explicit_name_conflict(&db, "milo").is_ok());
        // Row must survive — the launcher promotes it in place.
        assert!(db.get_instance("milo").unwrap().is_some());
    }

    #[test]
    fn resolve_explicit_name_conflict_rejects_active_row() {
        let db = launcher_test_db();
        insert_test_instance(&db, "rune", "listening");
        let err = resolve_explicit_name_conflict(&db, "rune")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "unexpected: {err}");
        // Row should still be present — no deletion on conflict.
        assert!(db.get_instance("rune").unwrap().is_some());
    }

    #[test]
    fn sidecar_ambient_env_folds_case_on_windows_only() {
        let mut env = std::collections::HashMap::new();
        env.insert("no_color".to_string(), "1".to_string());
        env.insert("NO_COLOR".to_string(), "1".to_string());
        env.insert("MY_SECRET".to_string(), "x".to_string());
        env.insert("HCOM_X".to_string(), "y".to_string());
        let strip = ["NO_COLOR"];
        let win = sidecar_ambient_env(&env, strip.iter().copied(), true);
        assert!(!win.contains_key("no_color") && !win.contains_key("NO_COLOR"));
        assert!(win.contains_key("MY_SECRET") && !win.contains_key("HCOM_X"));
        let unix = sidecar_ambient_env(&env, strip.iter().copied(), false);
        assert!(!unix.contains_key("NO_COLOR") && unix.contains_key("no_color")); // Unix exact-case preserved
    }
}
