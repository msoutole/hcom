//! Terminal launching, script creation, and process management.
//!
//!
//! Handles:
//! - Terminal preset resolution (kitty, wezterm, tmux, etc.)
//! - Bash script creation for tool launches
//! - Terminal process spawning (new window, same terminal, background)
//! - Kill/close operations for managed terminals

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};

use crate::shared::platform;

mod kill;
mod presets;
mod scripts;
mod spawn;
mod windows;

pub use kill::{KillResult, PaneCloseResult, close_terminal_pane, kill_process};
pub use presets::{
    TerminalInfo, detect_terminal_from_env, find_kitty_socket, get_available_presets,
    get_default_fallback_terminal_name, is_zellij_merged, is_zellij_preset, resolve_terminal_info,
    resolve_terminal_info_from_launch_context, resolve_terminal_mode_for_tips,
    resolve_terminal_open_argv, wezterm_reachable,
};
pub use scripts::{build_env_string, create_bash_script, create_powershell_script};
pub use spawn::{LaunchResult, launch_terminal};
pub use windows::{executable_command, resolve_windows_tool_launcher};

// Items defined directly in mod.rs stay naturally available:
// which_bin, which_candidates, ps_quote, shell_quote, has_node_shebang,
// resolve_termux_tool_launcher, TERMINAL_CONTEXT_VARS, TERMINAL_COLOR_VARS

// Internal-use re-exports for sibling submodules.
// Some re-exports are only used by sibling submodules via `super::`, which the
// compiler cannot see from this module, so suppress unused-import warnings.
#[allow(unused_imports)]
pub(crate) use presets::{
    find_kitten_binary, get_linux_terminal_argv, get_macos_terminal_argv,
    normalize_terminal_mode_for_launch, resolve_binary_path, resolve_kitty_remote_socket,
    should_use_command_extension,
};
#[allow(unused_imports)]
pub(crate) use windows::{
    get_windows_terminal_argv, windows_default_terminal_display_name, windows_shellify_custom_argv,
};

pub(crate) const TERMINAL_CONTEXT_VARS: &[&str] = &[
    // Multiplexers
    "CMUX_WORKSPACE_ID",
    "CMUX_SURFACE_ID",
    "TMUX_PANE",
    "ZELLIJ_PANE_ID",
    // GPU/rich terminals
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_LISTEN_ON",
    "WEZTERM_PANE",
    "WAVETERM_BLOCKID",
    // Bare terminal emulators
    "GHOSTTY_RESOURCES_DIR",
    "ITERM_SESSION_ID",
    "ALACRITTY_WINDOW_ID",
    "GNOME_TERMINAL_SCREEN",
    "KONSOLE_DBUS_WINDOW",
    "TERMINATOR_UUID",
    "TILIX_ID",
    "WT_SESSION",
    // HERDR_* not stripped: the herdr preset's CLI (`herdr agent start ...`)
    // resolves its server socket from $HERDR_SOCKET_PATH; other presets pass
    // the socket via CLI flag (e.g. `kitty @ --to {kitty_listen}`) and don't
    // need their identity vars to survive in the launcher env.
    // Generic terminal identity
    "TERM_PROGRAM",
    "TERM_SESSION_ID",
];

/// Terminal color/capability vars should come from the terminal/PTY that hosts
/// the child. Forwarding a parent's `TERM=dumb`, test-harness `NO_COLOR`, or
/// stale truecolor state into a new kitty pane can make otherwise-capable tools
/// render in black and white.
pub(crate) const TERMINAL_COLOR_VARS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "FORCE_COLOR",
];

/// Detect terminal preset from inherited environment variables.
/// Used for same-terminal PTY launches (run_here=True) to enable close-on-kill.
/// Checks built-in env map first, then TOML presets with pane_id_env defined.
pub(crate) fn which_candidates(dir: &Path, name: &str) -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        if Path::new(name).extension().is_some() {
            return vec![dir.join(name)];
        }
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut out: Vec<std::path::PathBuf> = exts
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|ext| dir.join(format!("{name}{ext}")))
            .collect();
        out.push(dir.join(name));
        out
    }
    #[cfg(not(windows))]
    {
        vec![dir.join(name)]
    }
}

/// Simple `which` implementation — find binary in PATH.
pub fn which_bin(name: &str) -> Option<String> {
    // `split_paths` uses the platform separator (`;` on Windows, `:` elsewhere),
    // which also avoids splitting Windows drive letters like `C:`. PATH being
    // entirely unset (rather than merely lacking `name`) still falls through
    // to the well-known-location fallbacks below.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for candidate in which_candidates(&dir, name) {
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().to_string());
                }
            }
        }
    }

    // Fallback: well-known install locations not always in PATH
    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        let fallbacks: &[std::path::PathBuf] = match name {
            "claude" => &[
                home.join(".claude").join("local").join("claude"),
                home.join(".local").join("bin").join("claude"),
                home.join(".claude").join("bin").join("claude"),
            ],
            "opencode" => &[home.join(".opencode").join("bin").join("opencode")],
            "kilo" => &[home.join(".kilo").join("bin").join("kilo")],
            "cursor-agent" => &[home.join(".local").join("bin").join("cursor-agent")],
            _ => &[],
        };
        for fallback in fallbacks {
            if fallback.exists() && fallback.is_file() {
                return Some(fallback.to_string_lossy().to_string());
            }
        }
    }

    #[cfg(windows)]
    if name.eq_ignore_ascii_case("agy")
        && let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
    {
        let fallback = Path::new(&local_app_data)
            .join("agy")
            .join("bin")
            .join("agy.exe");
        if fallback.is_file() {
            return Some(fallback.to_string_lossy().into_owned());
        }
    }

    None
}

/// Build a command for a PATH-resolved executable, including Windows npm
/// `.cmd`/`.bat` shims that CreateProcess cannot execute directly.
fn resolve_bash_command() -> String {
    which_bin("bash").unwrap_or_else(|| "/bin/bash".to_string())
}

/// Check if a file has a node shebang (#!/usr/bin/env node or similar).
/// Used on Termux to detect npm-installed tools that need `node <path>` rewrite.
pub fn has_node_shebang(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    let header = String::from_utf8_lossy(&buf[..n]);
    header.starts_with("#!") && header.contains("node")
}

const TERMUX_CODEX_WRAPPER_PATH: &str = "/data/data/com.termux/files/usr/bin/codex";
const TERMUX_CODEX_INNER_WRAPPER_PATH: &str =
    "/data/data/com.termux/files/usr/lib/node_modules/@mmmbuto/codex-cli-termux/bin/codex";
const TERMUX_SH_PATH: &str = "/data/data/com.termux/files/usr/bin/sh";
const TERMUX_PREFIX: &str = "/data/data/com.termux/files/usr";

/// Whether the host Termux installation is visible from this process.
///
/// This is intentionally broader than `is_native_termux_runtime()`: PRoot
/// exposes host Termux paths and environment variables inside the distro.
fn termux_host_visible() -> bool {
    platform::is_termux()
}

fn native_termux_prefix() -> bool {
    std::env::var("PREFIX")
        .ok()
        .is_some_and(|prefix| Path::new(&prefix) == Path::new(TERMUX_PREFIX))
}

fn proc_self_root_is_root() -> bool {
    fs::read_link("/proc/self/root").is_ok_and(|root| root == Path::new("/"))
}

fn is_native_termux_runtime_from(
    is_android_target: bool,
    host_visible: bool,
    has_native_prefix: bool,
    self_root_is_root: bool,
) -> bool {
    is_android_target && host_visible && has_native_prefix && self_root_is_root
}

/// Whether this process can safely dispatch scripts into native Termux.
fn is_native_termux_runtime() -> bool {
    is_native_termux_runtime_from(
        cfg!(target_os = "android"),
        termux_host_visible(),
        native_termux_prefix(),
        proc_self_root_is_root(),
    )
}

fn proot_termux_launch_error() -> &'static str {
    "Cannot open a native Termux window from this PRoot runtime:\n\
     the generated command uses PRoot paths and Linux/glibc binaries that cannot run\n\
     in host Termux.\n\n\
     Use one of:\n\
       hcom <tool> --headless\n\
       hcom <tool> --terminal tmux"
}

fn validate_termux_dispatch_status(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("Termux RUN_COMMAND dispatch failed: {status}")
    }
}

/// Resolve Termux-only tool launch quirks.
///
/// Most npm-installed tools can run as `node <wrapper.js> ...`, but the
/// third-party `codex-cli-termux` wrapper breaks in stripped `RUN_COMMAND`
/// environments when its JS wrapper tries to spawn the nested shell wrapper
/// directly. Bypass that path by invoking the inner wrapper with `sh`.
pub fn resolve_termux_tool_launcher(
    tool_name: &str,
    resolved: &str,
) -> Option<(String, Vec<String>)> {
    if !is_native_termux_runtime() {
        return None;
    }

    if tool_name == "codex"
        && resolved == TERMUX_CODEX_WRAPPER_PATH
        && Path::new(TERMUX_CODEX_INNER_WRAPPER_PATH).exists()
    {
        let sh = which_bin("sh").unwrap_or_else(|| TERMUX_SH_PATH.to_string());
        return Some((sh, vec![TERMUX_CODEX_INNER_WRAPPER_PATH.to_string()]));
    }

    if has_node_shebang(resolved) {
        let node = which_bin("node").unwrap_or_else(|| platform::TERMUX_NODE_PATH.to_string());
        return Some((node, vec![resolved.to_string()]));
    }

    None
}

/// Resolve binary to full path via macOS app bundle fallback.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // If all safe chars, no quoting needed
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/_-.=:,@".contains(c))
    {
        return s.to_string();
    }
    // Use single quotes, escaping any embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Resolve a human-readable tool name for the launch script title/banner,
/// detecting from the command when not explicitly provided.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Build sorted `$env:K = 'V'` assignments, applying the same key validation
/// as `build_env_string` so only well-formed names are emitted.
#[derive(Default, Clone, Copy)]
pub(crate) struct TerminalCommandContext<'a> {
    pub script: &'a str,
    pub process_id: &'a str,
    pub cwd: &'a str,
    pub instance_name: &'a str,
    pub tool: &'a str,
    /// Pre-formatted pane title (e.g. `◉ team-luna [claude]`). Falls back to
    /// `instance_name` when None or empty.
    pub pane_title: Option<&'a str>,
}

/// Substitute placeholders into an open-command argv template, per element.
///
/// Each element of `template` is one argument (no shell splitting). Placeholders
/// are replaced inside each element with `String::replace`, so a Windows path
/// like `C:\Users\x\s.ps1` substituted into the `{script}` element survives
/// intact (no backslash mangling, no re-quoting). Requires at least one element
/// to contain `{script}`.
fn substitute_open_argv(
    template: &[String],
    ctx: TerminalCommandContext<'_>,
) -> Result<Vec<String>> {
    if !template.iter().any(|p| p.contains("{script}")) {
        bail!(
            "Custom terminal command must include {{script}} placeholder\n\
             Example: open -n -a kitty.app --args bash \"{{script}}\""
        );
    }

    let pane_title = ctx
        .pane_title
        .filter(|s| !s.is_empty())
        .unwrap_or(ctx.instance_name);

    let replaced: Vec<String> = template
        .iter()
        .map(|part| {
            let mut part = part.clone();
            for (placeholder, value) in [
                ("{process_id}", ctx.process_id),
                ("{cwd}", ctx.cwd),
                ("{instance_name}", ctx.instance_name),
                ("{tool}", ctx.tool),
                ("{pane_title}", pane_title),
                ("{script}", ctx.script),
            ] {
                if part.contains(placeholder) {
                    part = part.replace(placeholder, value);
                }
            }
            part
        })
        .collect();

    Ok(replaced)
}

/// Substitute placeholders into a close-command argv template, per element.
///
/// Mirrors the open path but for close placeholders. Returns `None` (caller
/// treats as "skip the close") when a required placeholder appears in the
/// template but the corresponding value is empty — preserving the previous
/// `close_terminal_pane` skip semantics. `effective_pane_id` is the resolved
/// pane id (caller falls back from `pane_id` to `terminal_id`).
fn substitute_close_argv(
    template: &[String],
    pid: u32,
    effective_pane_id: &str,
    process_id: &str,
    terminal_id: &str,
) -> Option<Vec<String>> {
    let needs = |ph: &str| template.iter().any(|p| p.contains(ph));

    if needs("{pane_id}") && effective_pane_id.is_empty() {
        return None;
    }
    if needs("{process_id}") && process_id.is_empty() {
        return None;
    }
    if needs("{id}") && terminal_id.is_empty() {
        return None;
    }

    let pid_str = pid.to_string();
    let argv: Vec<String> = template
        .iter()
        .map(|part| {
            let mut part = part.clone();
            for (placeholder, value) in [
                ("{pid}", pid_str.as_str()),
                ("{pane_id}", effective_pane_id),
                ("{process_id}", process_id),
                ("{id}", terminal_id),
            ] {
                if part.contains(placeholder) {
                    part = part.replace(placeholder, value);
                }
            }
            part
        })
        .collect();

    Some(argv)
}

/// Get macOS Terminal.app launch argv ({script} substituted by the caller).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "hello");
    }

    #[test]
    fn test_shell_quote_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_quote_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("plain"), "'plain'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote(""), "''");
    }

    #[test]
    fn test_native_termux_runtime_requires_all_native_signals() {
        assert!(is_native_termux_runtime_from(true, true, true, true));
        assert!(!is_native_termux_runtime_from(false, true, true, true));
        assert!(!is_native_termux_runtime_from(true, false, true, true));
        assert!(!is_native_termux_runtime_from(true, true, false, true));
        assert!(!is_native_termux_runtime_from(true, true, true, false));
    }

    #[test]
    fn test_proot_termux_launch_error_is_actionable() {
        let message = proot_termux_launch_error();
        assert!(message.contains("--headless"));
        assert!(message.contains("--terminal tmux"));
    }

    #[test]
    #[cfg(unix)]
    fn test_termux_dispatch_rejects_nonzero_exit_status() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let err = validate_termux_dispatch_status(status)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Termux RUN_COMMAND dispatch failed"));
    }

    #[test]
    fn test_has_node_shebang_with_node_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.js");
        std::fs::write(&path, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();
        assert!(has_node_shebang(path.to_str().unwrap()));
    }

    #[test]
    fn test_has_node_shebang_with_bash_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.sh");
        std::fs::write(&path, "#!/bin/bash\necho hello\n").unwrap();
        assert!(!has_node_shebang(path.to_str().unwrap()));
    }

    #[test]
    fn test_has_node_shebang_with_elf_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, b"\x7fELF\x02\x01\x01\x00").unwrap();
        assert!(!has_node_shebang(path.to_str().unwrap()));
    }

    #[test]
    fn test_has_node_shebang_nonexistent() {
        assert!(!has_node_shebang("/nonexistent/path/to/tool"));
    }

    #[test]
    fn test_resolve_termux_tool_launcher_codex_wrapper() {
        let resolved = resolve_termux_tool_launcher("codex", TERMUX_CODEX_WRAPPER_PATH);
        if crate::shared::platform::is_termux()
            && Path::new(TERMUX_CODEX_INNER_WRAPPER_PATH).exists()
        {
            let (command, args) = resolved.expect("expected termux codex wrapper override");
            assert!(command.ends_with("/sh") || command == "sh");
            assert_eq!(args, vec![TERMUX_CODEX_INNER_WRAPPER_PATH.to_string()]);
        } else {
            assert!(resolved.is_none());
        }
    }

    #[test]
    fn test_resolve_termux_tool_launcher_node_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, "#!/usr/bin/env node\nconsole.log('ok');\n").unwrap();

        let resolved = resolve_termux_tool_launcher("tool", path.to_str().unwrap());
        if crate::shared::platform::is_termux() {
            let (command, args) = resolved.expect("expected node wrapper on termux");
            assert!(command.ends_with("/node") || command == "node");
            assert_eq!(args, vec![path.to_string_lossy().to_string()]);
        } else {
            assert!(resolved.is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bash_command_prefers_path_then_fallback() {
        let r = resolve_bash_command();
        assert!(r.ends_with("bash"));
        match which_bin("bash") {
            Some(p) => assert_eq!(r, p),
            None => assert_eq!(r, "/bin/bash"),
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn ctx_with_script(script: &str) -> TerminalCommandContext<'_> {
        TerminalCommandContext {
            script,
            ..TerminalCommandContext::default()
        }
    }

    #[test]
    fn test_substitute_open_argv_basic() {
        let out = substitute_open_argv(
            &argv(&["open", "-a", "Terminal", "{script}"]),
            ctx_with_script("/tmp/test.sh"),
        )
        .unwrap();
        assert_eq!(out, vec!["open", "-a", "Terminal", "/tmp/test.sh"]);
    }

    #[test]
    fn test_substitute_open_argv_preserves_windows_path() {
        let out = substitute_open_argv(
            &argv(&["wt", "--", "powershell", "-File", "{script}"]),
            ctx_with_script(r"C:\Users\x\s.ps1"),
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["wt", "--", "powershell", "-File", r"C:\Users\x\s.ps1"]
        );
    }

    #[test]
    fn test_substitute_open_argv_process_id_element() {
        let out = substitute_open_argv(
            &argv(&["kitty", "--env", "HCOM_PROCESS_ID={process_id}", "{script}"]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["kitty", "--env", "HCOM_PROCESS_ID=abc-123", "/tmp/test.sh"]
        );
    }

    #[test]
    fn test_substitute_open_argv_missing_placeholder() {
        assert!(
            substitute_open_argv(
                &argv(&["open", "-a", "Terminal"]),
                ctx_with_script("/tmp/test.sh")
            )
            .is_err()
        );
    }

    #[test]
    fn test_substitute_open_argv_with_process_id() {
        let out = substitute_open_argv(
            &argv(&["tmux", "split", "-t", "{process_id}", "--", "{script}"]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["tmux", "split", "-t", "abc-123", "--", "/tmp/test.sh"]
        );
    }

    #[test]
    fn test_substitute_herdr_open_argv_uses_pane_title() {
        let template = argv(&[
            "herdr",
            "agent",
            "start",
            "{pane_title}",
            "--cwd",
            "{cwd}",
            "--no-focus",
            "--",
            "bash",
            "{script}",
        ]);
        let out = substitute_open_argv(
            &template,
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                cwd: "/home/user/project",
                instance_name: "luna",
                tool: "claude",
                pane_title: Some("\u{25c9} luna [claude]"),
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "herdr",
                "agent",
                "start",
                "\u{25c9} luna [claude]",
                "--cwd",
                "/home/user/project",
                "--no-focus",
                "--",
                "bash",
                "/tmp/test.sh"
            ]
        );
    }

    #[test]
    fn test_substitute_open_argv_pane_title_falls_back_to_instance_name() {
        let out = substitute_open_argv(
            &argv(&[
                "herdr",
                "agent",
                "start",
                "{pane_title}",
                "--",
                "bash",
                "{script}",
            ]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                instance_name: "abc-123",
                tool: "codex",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "herdr",
                "agent",
                "start",
                "abc-123",
                "--",
                "bash",
                "/tmp/test.sh"
            ]
        );
    }

    #[test]
    fn test_substitute_open_argv_cwd_placeholder() {
        let out = substitute_open_argv(
            &argv(&["myterm", "--dir", "{cwd}", "--", "bash", "{script}"]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                cwd: "/home/user",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "myterm",
                "--dir",
                "/home/user",
                "--",
                "bash",
                "/tmp/test.sh"
            ]
        );
    }

    #[test]
    fn test_substitute_open_argv_empty_cwd() {
        let out = substitute_open_argv(
            &argv(&["open", "-a", "Terminal", "{script}"]),
            ctx_with_script("/tmp/test.sh"),
        )
        .unwrap();
        assert_eq!(out, vec!["open", "-a", "Terminal", "/tmp/test.sh"]);
    }

    #[test]
    fn test_substitute_close_argv_skips_when_pane_id_missing() {
        assert!(
            substitute_close_argv(
                &argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
                42,
                "",
                "proc-1",
                "",
            )
            .is_none()
        );
    }

    #[test]
    fn test_substitute_close_argv_substitutes_pane_id() {
        let out = substitute_close_argv(
            &argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
            42,
            "pane-7",
            "proc-1",
            "",
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["wezterm", "cli", "kill-pane", "--pane-id", "pane-7"]
        );
    }
}
