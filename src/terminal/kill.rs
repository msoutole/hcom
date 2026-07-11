//! Kill/close operations for managed terminals.

use std::process::Command;
use std::time::{Duration, Instant};

use super::presets::is_zellij_merged;
use super::{find_kitten_binary, ps_quote, resolve_binary_path, substitute_close_argv};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillResult {
    Sent,
    AlreadyDead,
    PermissionDenied,
}

const TERMINAL_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCloseResult {
    pub closed: bool,
    pub retry_command: Option<String>,
}

fn format_close_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            #[cfg(windows)]
            {
                if !arg.is_empty()
                    && arg
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "/_-.=:,@\\".contains(c))
                {
                    arg.clone()
                } else {
                    ps_quote(arg)
                }
            }
            #[cfg(not(windows))]
            {
                crate::tools::args_common::shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Terminal info resolved for an instance.
pub fn close_terminal_pane(
    pid: u32,
    preset_name: &str,
    pane_id: &str,
    process_id: &str,
    kitty_listen_on: &str,
    terminal_id: &str,
    zellij_session_name: &str,
) -> PaneCloseResult {
    let failed_without_command = || PaneCloseResult {
        closed: false,
        retry_command: None,
    };
    let merged = match crate::config::get_merged_preset(preset_name) {
        Some(p) => p,
        None => return failed_without_command(),
    };

    let close_template = match merged.close_argv(cfg!(windows)) {
        Some(c) => c,
        None => return failed_without_command(),
    };

    // Determine effective pane_id (fall back to terminal_id)
    let effective_pane_id = if pane_id.is_empty() && !terminal_id.is_empty() {
        terminal_id
    } else {
        pane_id
    };

    // Substitute close placeholders per-element. Returns None when a required
    // placeholder is present but its value is empty (skip the close).
    let mut argv = match substitute_close_argv(
        &close_template,
        pid,
        effective_pane_id,
        process_id,
        terminal_id,
    ) {
        Some(a) => a,
        None => return failed_without_command(),
    };

    let is_zellij = is_zellij_merged(&merged);

    let zellij_before_close = if is_zellij {
        match zellij_terminal_pane_exists(zellij_session_name, effective_pane_id) {
            Some(true) => Some(true),
            Some(false) => return failed_without_command(),
            None => None,
        }
    } else {
        None
    };

    // Splice `--session <name>` right after `zellij` for `zellij action ...`.
    if is_zellij
        && !zellij_session_name.is_empty()
        && argv.first().map(String::as_str) == Some("zellij")
        && argv.get(1).map(String::as_str) == Some("action")
    {
        argv.splice(
            1..1,
            ["--session".to_string(), zellij_session_name.to_string()],
        );
    }

    // Inject `--to <socket>` (after the `@`) for kitten commands when we have
    // the socket path. Must run before the binary-path rewrite below, because
    // that rewrite replaces argv[0] with an absolute path and the "kitten"
    // string check would no longer match.
    if argv.first().map(String::as_str) == Some("kitten")
        && argv.get(1).map(String::as_str) == Some("@")
        && !kitty_listen_on.is_empty()
        && !argv.iter().any(|a| a == "--to")
        && !kitty_listen_on.starts_with("fd:")
    {
        argv.splice(2..2, ["--to".to_string(), kitty_listen_on.to_string()]);
    }

    // Resolve binary path via app bundle fallback (replace argv[0]).
    if let Some(ref binary) = merged.binary {
        let app_name = merged.app_name.as_deref().unwrap_or(preset_name);
        if let Some(full_path) = resolve_binary_path(binary, Some(app_name), preset_name)
            && argv.first().map(String::as_str) == Some(binary.as_str())
        {
            argv[0] = full_path;
        }
    }
    if argv.first().map(String::as_str) == Some("kitten")
        && let Some(full_path) = find_kitten_binary()
    {
        argv[0] = full_path;
    }

    if argv.is_empty() {
        return failed_without_command();
    }
    let retry_command = format_close_command(&argv);
    let failed = || PaneCloseResult {
        closed: false,
        retry_command: Some(retry_command.clone()),
    };

    // Run the close command directly (no shell) so it works on Windows too.
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("Failed to close {preset_name} pane: {err}");
            return failed();
        }
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TERMINAL_CLOSE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                eprintln!(
                    "Timed out after {}s closing {preset_name} pane {effective_pane_id}",
                    TERMINAL_CLOSE_TIMEOUT.as_secs()
                );
                return failed();
            }
            Err(err) => {
                let _ = child.kill();
                eprintln!("Failed waiting for {preset_name} pane close: {err}");
                return failed();
            }
        }
    };

    if !status.success() {
        eprintln!("Failed to close {preset_name} pane {effective_pane_id}: {status}");
        return failed();
    }

    if is_zellij {
        let closed = zellij_before_close == Some(true)
            && zellij_terminal_pane_exists(zellij_session_name, effective_pane_id) == Some(false);
        return PaneCloseResult {
            closed,
            retry_command: (!closed).then_some(retry_command),
        };
    }

    PaneCloseResult {
        closed: true,
        retry_command: None,
    }
}

fn zellij_terminal_pane_exists(session_name: &str, pane_id: &str) -> Option<bool> {
    let pane_num = pane_id
        .strip_prefix("terminal_")
        .unwrap_or(pane_id)
        .parse::<i64>()
        .ok()?;

    let mut command = Command::new("zellij");
    if !session_name.is_empty() {
        command.args(["--session", session_name]);
    }
    let output = command
        .args(["action", "list-panes", "--json", "--all"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let panes = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok()?;
    let panes = panes.as_array()?;
    Some(panes.iter().any(|pane| {
        pane.get("is_plugin").and_then(|v| v.as_bool()) == Some(false)
            && pane.get("id").and_then(|v| v.as_i64()) == Some(pane_num)
    }))
}

/// Close terminal pane (if applicable) then SIGTERM the process group.
pub fn kill_process(
    pid: u32,
    preset_name: &str,
    pane_id: &str,
    process_id: &str,
    kitty_listen_on: &str,
    terminal_id: &str,
    zellij_session_name: &str,
) -> (KillResult, bool, Option<String>) {
    let pane_close = if !preset_name.is_empty() {
        close_terminal_pane(
            pid,
            preset_name,
            pane_id,
            process_id,
            kitty_listen_on,
            terminal_id,
            zellij_session_name,
        )
    } else {
        PaneCloseResult {
            closed: false,
            retry_command: None,
        }
    };

    // SIGTERM the process group
    use crate::sys::process::GroupSignal;
    let kill_result = match crate::sys::process::terminate_group(pid) {
        GroupSignal::Sent => KillResult::Sent,
        #[cfg(unix)]
        GroupSignal::PermissionDenied => KillResult::PermissionDenied,
        GroupSignal::NotFound => KillResult::AlreadyDead,
        #[cfg(unix)]
        GroupSignal::Other => KillResult::AlreadyDead,
    };

    (kill_result, pane_close.closed, pane_close.retry_command)
}

/// Resolve terminal info from the canonical preset fields plus launch_context metadata.

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_format_close_command_preserves_all_arguments() {
        let command = format_close_command(&[
            "wezterm".to_string(),
            "cli".to_string(),
            "kill-pane".to_string(),
            "--pane-id".to_string(),
            "123".to_string(),
        ]);
        assert_eq!(command, "wezterm cli kill-pane --pane-id 123");
    }

    #[cfg(windows)]
    #[test]
    fn test_format_close_command_quotes_powershell_arguments() {
        let command = format_close_command(&[
            r"C:\Program Files\kitty\kitten.exe".to_string(),
            "@".to_string(),
            "--to".to_string(),
            r"unix:C:\Users\O'Brien\kitty.sock".to_string(),
        ]);
        assert_eq!(
            command,
            r#"'C:\Program Files\kitty\kitten.exe' @ --to 'unix:C:\Users\O''Brien\kitty.sock'"#
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_format_close_command_quotes_posix_arguments() {
        let command = format_close_command(&[
            "kitten".to_string(),
            "@".to_string(),
            "--to".to_string(),
            "/tmp/O'Brien kitty.sock".to_string(),
        ]);
        assert_eq!(command, "kitten '@' --to '/tmp/O'\\''Brien kitty.sock'");
    }

    #[test]
    fn test_zellij_close_argv_session_splice() {
        let mut a = super::super::substitute_close_argv(
            &argv(&["zellij", "action", "close-pane", "--pane-id", "{pane_id}"]),
            0,
            "6",
            "",
            "",
        )
        .unwrap();
        a.splice(1..1, ["--session".to_string(), "wise-kangaroo".to_string()]);
        assert_eq!(
            a,
            vec![
                "zellij",
                "--session",
                "wise-kangaroo",
                "action",
                "close-pane",
                "--pane-id",
                "6"
            ]
        );
    }

    #[test]
    fn test_kitten_close_argv_to_splice() {
        let mut a = super::super::substitute_close_argv(
            &argv(&["kitten", "@", "close-window", "--match", "id:{pane_id}"]),
            0,
            "13",
            "",
            "",
        )
        .unwrap();
        a.splice(2..2, ["--to".to_string(), "unix:/tmp/kitty".to_string()]);
        assert_eq!(
            a,
            vec![
                "kitten",
                "@",
                "--to",
                "unix:/tmp/kitty",
                "close-window",
                "--match",
                "id:13"
            ]
        );
    }
}
