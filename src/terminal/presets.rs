//! Terminal preset resolution, detection, and macOS/Linux argv helpers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{which_bin, windows_default_terminal_display_name};
use crate::shared::platform;
use crate::shared::terminal_presets::{ArgvTemplate, TERMINAL_ENV_MAP};

#[derive(Debug, Clone, Default)]
pub struct TerminalInfo {
    pub preset_name: String,
    pub pane_id: String,
    pub process_id: String,
    pub kitty_listen_on: String,
    pub terminal_id: String,
    pub zellij_session_name: String,
}

/// Result from launch_terminal.
const MACOS_APP_FALLBACKS: &[(&str, ArgvTemplate)] = &[
    (
        "kitty-window",
        &["open", "-n", "-a", "kitty.app", "--args", "{script}"],
    ),
    (
        "wezterm-window",
        &[
            "open",
            "-n",
            "-a",
            "WezTerm.app",
            "--args",
            "start",
            "--",
            "bash",
            "{script}",
        ],
    ),
    (
        "alacritty",
        &[
            "open",
            "-n",
            "-a",
            "Alacritty.app",
            "--args",
            "-e",
            "bash",
            "{script}",
        ],
    ),
];

/// Terminal context vars stripped from the env before spawning a terminal launcher subprocess.
/// Prevents outer terminal identity from leaking into newly-launched terminal panes.
/// Must stay in sync with every env var read by detect_terminal_from_env().
pub fn detect_terminal_from_env() -> Option<String> {
    // Built-in mappings
    for &(env_var, preset_name) in TERMINAL_ENV_MAP {
        if std::env::var(env_var)
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            return Some(preset_name.to_string());
        }
    }
    // TOML-defined presets with pane_id_env
    let toml_path = crate::paths::config_toml_path();
    if let Some(presets_val) = crate::config::load_toml_presets(&toml_path)
        && let Some(table) = presets_val.as_table()
    {
        for (name, val) in table {
            if let Some(env_var) = val.get("pane_id_env").and_then(|v| v.as_str())
                && std::env::var(env_var)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .is_some()
            {
                return Some(name.clone());
            }
        }
    }
    // TERM_PROGRAM value-based detection (terminals without a unique env var)
    if let Ok(term_prog) = std::env::var("TERM_PROGRAM") {
        match term_prog.as_str() {
            "ghostty" => return Some("ghostty".to_string()),
            "iTerm.app" => return Some("iterm".to_string()),
            "Apple_Terminal" => return Some("terminal.app".to_string()),
            "WarpTerminal" => return Some("warp".to_string()),
            _ => {}
        }
    }
    None
}

/// Find macOS .app bundle in common locations.
fn find_macos_app(name: &str) -> Option<PathBuf> {
    let app_name = if name.ends_with(".app") {
        name.to_string()
    } else {
        format!("{}.app", name)
    };

    let home = std::env::var("HOME").ok()?;
    let search_dirs = [
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        PathBuf::from(home).join("Applications"),
    ];

    for base in &search_dirs {
        let app_path = base.join(&app_name);
        if app_path.exists() {
            return Some(app_path);
        }
    }
    None
}

/// Replace `open -a <app>` app names with absolute `.app` bundle paths, in place
/// on an argv vector.
///
/// This is only safe for app-launch commands where `open` passes argv via
/// `--args`. Plain file-open forms like `open -a Terminal {script}` must keep
/// `-a`, otherwise `open` treats the app bundle and script as regular paths and
/// falls back to file association for the script. No-ops if no `--args` tail is
/// present or no app flag is found.
fn rewrite_open_argv_with_app_path(argv: &mut Vec<String>, app_path: &Path) {
    for idx in 0..argv.len().saturating_sub(1) {
        let flag = &argv[idx];
        let takes_app_arg = flag == "-a"
            || (flag.starts_with('-')
                && !flag.starts_with("--")
                && flag.chars().skip(1).any(|c| c == 'a'));
        if takes_app_arg {
            let has_args_tail = argv.iter().skip(idx + 2).any(|part| part == "--args");
            if !has_args_tail {
                return;
            }
            let app_path_str = app_path.to_string_lossy().to_string();
            if flag == "-a" {
                argv.remove(idx);
                argv[idx] = app_path_str;
            } else {
                let mut rewritten_flag = String::from("-");
                for ch in flag.chars().skip(1) {
                    if ch != 'a' {
                        rewritten_flag.push(ch);
                    }
                }
                if rewritten_flag == "-" {
                    argv.remove(idx);
                    argv[idx] = app_path_str;
                } else {
                    argv[idx] = rewritten_flag;
                    argv[idx + 1] = app_path_str;
                }
            }
            return;
        }
    }
}

fn rewrite_macos_open_app_argv(argv: &mut Vec<String>, app_name: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if let Some(app_path) = find_macos_app(app_name) {
        rewrite_open_argv_with_app_path(argv, &app_path);
    }
}

pub(crate) fn should_use_command_extension(background: bool, terminal_mode: &str) -> bool {
    !background
        && cfg!(target_os = "macos")
        && (terminal_mode == "default" || terminal_mode == "terminal.app")
}

/// Find kitten binary — PATH first, then macOS app bundle.
pub(crate) fn find_kitten_binary() -> Option<String> {
    if let Some(path) = which_bin("kitten") {
        return Some(path);
    }
    if cfg!(target_os = "macos")
        && let Some(app) = find_macos_app("kitty")
    {
        let full = app.join("Contents/MacOS/kitten");
        if full.exists() {
            return Some(full.to_string_lossy().to_string());
        }
    }
    None
}

/// Find a reachable kitty remote control socket.
pub fn find_kitty_socket() -> String {
    let kitten = match find_kitten_binary() {
        Some(k) => k,
        None => return String::new(),
    };

    // Find candidate sockets
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("kitty") {
                candidates.push(entry.path());
            }
        }
    }
    candidates.sort_by(|a, b| b.cmp(a)); // Reverse sort (newest first)

    for sock_path in &candidates {
        // Skip anything that isn't a Unix-domain socket
        if !crate::sys::fs::is_socket(sock_path) {
            continue;
        }

        let socket_uri = format!("unix:{}", sock_path.display());
        if let Ok(output) = Command::new(&kitten)
            .args(["@", "--to", &socket_uri, "ls"])
            .output()
            && output.status.success()
        {
            return socket_uri;
        }
    }
    String::new()
}

pub(crate) fn resolve_kitty_remote_socket(kitty_socket: &str) -> String {
    if !kitty_socket.is_empty() {
        return kitty_socket.to_string();
    }
    std::env::var("KITTY_LISTEN_ON")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(find_kitty_socket)
}

pub(crate) fn normalize_terminal_mode_for_launch(
    mut terminal_mode: String,
    opens_new_window: bool,
    run_here: bool,
) -> (String, String) {
    let mut kitty_socket = String::new();

    if opens_new_window {
        if terminal_mode == "default"
            && let Some(detected) = detect_terminal_from_env()
        {
            terminal_mode = detected;
        }
        if terminal_mode == "kitty" {
            if std::env::var("KITTY_WINDOW_ID")
                .ok()
                .filter(|v| !v.is_empty())
                .is_some()
            {
                // Inside kitty — use split, but still need socket for --to injection
                kitty_socket = resolve_kitty_remote_socket(&kitty_socket);
                terminal_mode = "kitty-split".to_string();
            } else {
                kitty_socket = find_kitty_socket();
                terminal_mode = if kitty_socket.is_empty() {
                    "kitty-window".to_string()
                } else {
                    "kitty-tab".to_string()
                };
            }
        } else if terminal_mode == "wezterm" {
            if std::env::var("WEZTERM_PANE")
                .ok()
                .filter(|v| !v.is_empty())
                .is_some()
            {
                terminal_mode = "wezterm-split".to_string();
            } else if wezterm_reachable() {
                terminal_mode = "wezterm-tab".to_string();
            } else {
                terminal_mode = "wezterm-window".to_string();
            }
        }

        if terminal_mode == "kitty-tab" || terminal_mode == "kitty-split" {
            kitty_socket = resolve_kitty_remote_socket(&kitty_socket);
        }
    } else if run_here {
        if let Some(detected) = detect_terminal_from_env() {
            terminal_mode = detected;
        } else if terminal_mode == "here" {
            terminal_mode = "default".to_string();
        }
    }

    (terminal_mode, kitty_socket)
}

pub fn resolve_terminal_mode_for_tips(
    terminal: Option<&str>,
    config_terminal: &str,
    background: bool,
    run_here: bool,
) -> (String, bool) {
    let explicit_terminal = terminal.filter(|t| !t.is_empty()).or_else(|| {
        (config_terminal != "default" && !config_terminal.is_empty()).then_some(config_terminal)
    });

    let requested = explicit_terminal.unwrap_or("default").to_string();
    let (resolved, _) =
        normalize_terminal_mode_for_launch(requested, !background && !run_here, run_here);

    (
        resolved.clone(),
        explicit_terminal.is_none() && resolved != "default",
    )
}

/// Check if a wezterm mux server is reachable.
pub fn wezterm_reachable() -> bool {
    Command::new("wezterm")
        .args(["cli", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Candidate file names to probe for `name` in a PATH directory. On Windows an
/// extension-less name is expanded with PATHEXT (`.exe`, `.cmd`, …); elsewhere
/// the name is used verbatim.
pub(crate) fn resolve_binary_path(
    binary: &str,
    app_name: Option<&str>,
    preset_name: &str,
) -> Option<String> {
    if which_bin(binary).is_some() {
        return None; // Already on PATH
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    let app = find_macos_app(app_name.unwrap_or(preset_name))?;
    let full_path = app.join("Contents/MacOS").join(binary);
    if full_path.exists() {
        Some(full_path.to_string_lossy().to_string())
    } else {
        None
    }
}

/// True if `tok` names a bash-family interpreter — its file stem is `bash`
/// (case-insensitive), covering `bash`, `bash.exe`, `/bin/bash`, and
/// `C:\...\bash.exe`.
pub fn resolve_terminal_open_argv(preset_name: &str) -> Option<Vec<String>> {
    let merged = crate::config::get_merged_preset(preset_name)?;
    let mut open_argv = merged.open_argv(cfg!(windows));
    let app_name = merged.app_name.as_deref().unwrap_or(preset_name);

    if let Some(ref binary) = merged.binary
        && which_bin(binary).is_none()
        && cfg!(target_os = "macos")
    {
        // New-window presets have hardcoded fallbacks using `open -a`
        for &(name, fallback) in MACOS_APP_FALLBACKS {
            if name == preset_name && find_macos_app(app_name).is_some() {
                let mut argv: Vec<String> = fallback.iter().map(|s| s.to_string()).collect();
                rewrite_macos_open_app_argv(&mut argv, app_name);
                return Some(argv);
            }
        }
        // Tab/split presets: substitute leading binary element with full path
        if let Some(full_path) = resolve_binary_path(binary, Some(app_name), preset_name)
            && open_argv.first().map(String::as_str) == Some(binary.as_str())
        {
            open_argv[0] = full_path;
        }
    }

    rewrite_macos_open_app_argv(&mut open_argv, app_name);
    Some(open_argv)
}

/// Get terminal presets for current platform with availability status.
pub fn get_available_presets() -> Vec<(String, bool)> {
    let mut result = vec![("default".to_string(), true)];
    let system = platform::platform_name();
    let mut seen = std::collections::HashSet::new();

    for (name, preset) in crate::shared::terminal_presets::TERMINAL_PRESETS.iter() {
        if !preset.platforms.contains(&system) {
            continue;
        }

        let available = if let Some(binary) = preset.binary {
            let in_path = which_bin(binary).is_some();
            if !in_path && system == "Darwin" {
                resolve_binary_path(binary, preset.app_name, name).is_some()
            } else {
                in_path
            }
        } else if system == "Darwin" {
            let app_name = preset.app_name.unwrap_or(name);
            find_macos_app(app_name).is_some()
        } else {
            true
        };

        result.push((name.to_string(), available));
        seen.insert(name.to_string());
    }

    // Add TOML-defined presets not already in built-ins
    let toml_path = crate::paths::config_toml_path();
    if let Some(presets_val) = crate::config::load_toml_presets(&toml_path)
        && let Some(table) = presets_val.as_table()
    {
        for (name, preset_val) in table {
            if seen.contains(name) {
                continue;
            }
            let available = preset_val
                .get("binary")
                .and_then(|v| v.as_str())
                .map(|b| which_bin(b).is_some())
                .unwrap_or(true);
            result.push((name.clone(), available));
        }
    }

    result.push(("custom".to_string(), true));
    result
}

/// Build environment variable string for bash shells.
pub(crate) fn get_macos_terminal_argv() -> Vec<String> {
    let mut argv: Vec<String> = ["open", "-a", "Terminal", "{script}"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    rewrite_macos_open_app_argv(&mut argv, "Terminal");
    argv
}

/// Escape a string for use inside a YAML double-quoted scalar.
pub fn get_default_fallback_terminal_name() -> &'static str {
    if platform::is_termux() {
        return "Termux";
    }
    match platform::platform_name() {
        "Darwin" => "Terminal.app",
        "Linux" => {
            if platform::is_wsl() {
                if which_bin("wt.exe").is_some() {
                    "Windows Terminal"
                } else {
                    "cmd.exe"
                }
            } else if which_bin("gnome-terminal").is_some() {
                "gnome-terminal"
            } else if which_bin("konsole").is_some() {
                "konsole"
            } else if which_bin("xterm").is_some() {
                "xterm"
            } else {
                "none"
            }
        }
        "Windows" => windows_default_terminal_display_name(which_bin("wt").is_some()),
        _ => "unknown",
    }
}

/// Get first available standard Linux terminal.
pub(crate) fn get_linux_terminal_argv() -> Option<Vec<String>> {
    let terminals = [
        (
            "gnome-terminal",
            &["gnome-terminal", "--", "bash", "{script}"] as &[&str],
        ),
        ("konsole", &["konsole", "-e", "bash", "{script}"]),
        ("xterm", &["xterm", "-e", "bash", "{script}"]),
    ];

    for (term_name, argv) in &terminals {
        if which_bin(term_name).is_some() {
            return Some(argv.iter().map(|s| s.to_string()).collect());
        }
    }

    // WSL fallback
    if platform::is_wsl() && which_bin("cmd.exe").is_some() {
        if which_bin("wt.exe").is_some() {
            return Some(
                ["cmd.exe", "/c", "start", "wt.exe", "--", "bash", "{script}"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            );
        }
        return Some(
            ["cmd.exe", "/c", "start", "bash", "{script}"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }

    None
}

/// Default Windows terminal launch argv template ({script} substituted by
/// the caller). Host-testable: takes `has_wt` explicitly instead of probing.
///
/// Prefers Windows Terminal, which parses everything after `--` as a literal
/// argv so a script path with spaces stays a single argument. Without it, opens
/// a fresh console via cmd's `start` (the empty arg is start's window-title
/// slot; `{script}` is bare — `Command`'s Windows argv quoting adds quotes
/// only when needed). Either way the shell runs the generated `.ps1` with the
/// execution policy bypassed and stays open (`-NoExit`) for the new window.
pub fn is_zellij_preset(preset_name: &str) -> bool {
    if preset_name == "zellij" {
        return true;
    }
    crate::config::get_merged_preset(preset_name).is_some_and(|p| is_zellij_merged(&p))
}

pub fn is_zellij_merged(preset: &crate::config::MergedPreset) -> bool {
    let is_zellij_argv0 = |argv: &[String]| argv.first().map(String::as_str) == Some("zellij");
    preset.binary.as_deref() == Some("zellij")
        || is_zellij_argv0(&preset.open)
        || preset.open_windows.as_deref().is_some_and(is_zellij_argv0)
        || preset.close.as_deref().is_some_and(is_zellij_argv0)
        || preset.close_windows.as_deref().is_some_and(is_zellij_argv0)
}

pub fn resolve_terminal_info(
    preset_name: Option<&str>,
    launch_context_json: Option<&str>,
) -> TerminalInfo {
    let mut info = TerminalInfo {
        preset_name: preset_name.unwrap_or("").to_string(),
        ..TerminalInfo::default()
    };

    if let Some(launch_context_json) = launch_context_json.filter(|s| !s.is_empty())
        && let Ok(lc) = serde_json::from_str::<serde_json::Value>(launch_context_json)
    {
        if info.preset_name.is_empty() {
            info.preset_name = lc
                .get("terminal_preset_effective")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    lc.get("terminal_preset")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or("")
                .to_string();
        }
        info.pane_id = lc
            .get("pane_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        info.process_id = lc
            .get("process_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        info.terminal_id = lc
            .get("terminal_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if is_zellij_preset(&info.preset_name)
            && let Some(pane_id) = zellij_pane_id_from_terminal_id(&info.terminal_id)
        {
            info.pane_id = pane_id;
        }
        // Kitty socket from launch context or env snapshot
        let lc_env = lc.get("env").and_then(|v| v.as_object());
        info.kitty_listen_on = lc
            .get("kitty_listen_on")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| lc_env.and_then(|e| e.get("KITTY_LISTEN_ON").and_then(|v| v.as_str())))
            .unwrap_or("")
            .to_string();
        info.zellij_session_name = lc_env
            .and_then(|e| e.get("ZELLIJ_SESSION_NAME").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
    }

    // Legacy kitty launches may have pane/socket metadata but no persisted preset.
    // Both kitty-tab and kitty-split now close via close-window on the captured ID,
    // so treating these old records as kitty-split is sufficient for cleanup.
    if info.preset_name.is_empty() && !info.pane_id.is_empty() && !info.kitty_listen_on.is_empty() {
        info.preset_name = "kitty-split".to_string();
    }

    info
}

/// Parse only launch_context metadata. Prefer `resolve_terminal_info()` for runtime decisions.
pub fn resolve_terminal_info_from_launch_context(launch_context_json: &str) -> TerminalInfo {
    resolve_terminal_info(None, Some(launch_context_json))
}

fn zellij_pane_id_from_terminal_id(terminal_id: &str) -> Option<String> {
    terminal_id
        .strip_prefix("terminal_")
        .filter(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
        .map(|suffix| suffix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::Path;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    struct EnvGuard(Vec<(&'static str, Option<String>)>);

    impl EnvGuard {
        fn clear(vars: &'static [&'static str]) -> Self {
            let saved = vars
                .iter()
                .map(|&var| (var, std::env::var(var).ok()))
                .collect::<Vec<_>>();
            for &var in vars {
                unsafe {
                    std::env::remove_var(var);
                }
            }
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (var, value) in &self.0 {
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(var, value);
                    } else {
                        std::env::remove_var(var);
                    }
                }
            }
        }
    }

    const DETECT_ONLY_VARS: &[&str] = &["HERDR_PANE_ID", "HERDR_SOCKET_PATH", "HERDR_ENV"];

    #[test]
    fn test_detect_terminal_from_env_none() {
        let result = detect_terminal_from_env();
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_normalize_terminal_mode_for_launch_resolves_socket_for_auto_detected_kitty() {
        let _env = EnvGuard::clear(super::super::TERMINAL_CONTEXT_VARS);
        let _detect = EnvGuard::clear(DETECT_ONLY_VARS);
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "window-1");
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty-test");
        }

        let (mode, socket) = normalize_terminal_mode_for_launch("default".to_string(), true, false);

        assert_eq!(mode, "kitty-split");
        assert_eq!(socket, "unix:/tmp/kitty-test");
    }

    #[test]
    #[serial]
    fn test_resolve_terminal_mode_for_tips_uses_normalized_auto_detected_mode() {
        let _env = EnvGuard::clear(super::super::TERMINAL_CONTEXT_VARS);
        let _detect = EnvGuard::clear(DETECT_ONLY_VARS);
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "window-1");
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty-test");
        }

        let (mode, auto) = resolve_terminal_mode_for_tips(None, "default", false, false);

        assert_eq!(mode, "kitty-split");
        assert!(auto);
    }

    #[test]
    fn test_rewrite_open_argv_with_app_path_keeps_plain_open_a() {
        let mut v = argv(&["open", "-a", "Terminal", "{script}"]);
        rewrite_open_argv_with_app_path(
            &mut v,
            Path::new("/System/Applications/Utilities/Terminal.app"),
        );
        assert_eq!(v, vec!["open", "-a", "Terminal", "{script}"]);
    }

    #[test]
    fn test_rewrite_open_argv_with_combined_flag() {
        let mut v = argv(&[
            "open",
            "-na",
            "Ghostty.app",
            "--args",
            "-e",
            "bash",
            "{script}",
        ]);
        rewrite_open_argv_with_app_path(&mut v, Path::new("/Applications/Ghostty.app"));
        assert_eq!(
            v,
            vec![
                "open",
                "-n",
                "/Applications/Ghostty.app",
                "--args",
                "-e",
                "bash",
                "{script}"
            ]
        );
    }

    #[test]
    fn test_rewrite_open_argv_with_explicit_args() {
        let mut v = argv(&["open", "-a", "Terminal", "--args", "bash", "{script}"]);
        rewrite_open_argv_with_app_path(
            &mut v,
            Path::new("/System/Applications/Utilities/Terminal.app"),
        );
        assert_eq!(
            v,
            vec![
                "open",
                "/System/Applications/Utilities/Terminal.app",
                "--args",
                "bash",
                "{script}"
            ]
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_should_use_command_extension_for_terminal_app() {
        assert!(should_use_command_extension(false, "default"));
        assert!(should_use_command_extension(false, "terminal.app"));
        assert!(!should_use_command_extension(false, "iterm"));
        assert!(!should_use_command_extension(true, "terminal.app"));
    }

    #[test]
    fn test_resolve_terminal_info_prefers_effective_preset() {
        let info = resolve_terminal_info(Some("kitty-tab"), Some(r#"{"pane_id":"x"}"#));
        assert_eq!(info.preset_name, "kitty-tab");
    }

    #[test]
    fn test_resolve_terminal_info_reads_launch_context_metadata() {
        let info = resolve_terminal_info(
            Some("wezterm-split"),
            Some(r#"{"pane_id":"pane-1","process_id":"proc-1","terminal_id":"term-1"}"#),
        );
        assert_eq!(info.preset_name, "wezterm-split");
        assert_eq!(info.pane_id, "pane-1");
        assert_eq!(info.process_id, "proc-1");
        assert_eq!(info.terminal_id, "term-1");
    }

    #[test]
    fn test_resolve_terminal_info_prefers_zellij_terminal_id_over_env_pane_id() {
        let info = resolve_terminal_info(
            Some("zellij"),
            Some(r#"{"pane_id":"18","terminal_id":"terminal_6","process_id":"proc-1"}"#),
        );

        assert_eq!(info.pane_id, "6");
        assert_eq!(info.terminal_id, "terminal_6");
    }

    #[test]
    fn test_resolve_terminal_info_uses_launch_context_preset_when_column_missing() {
        let info = resolve_terminal_info(
            None,
            Some(
                r#"{"terminal_preset_effective":"kitty-tab","pane_id":"pane-1","kitty_listen_on":"unix:/tmp/kitty"}"#,
            ),
        );
        assert_eq!(info.preset_name, "kitty-tab");
        assert_eq!(info.pane_id, "pane-1");
        assert_eq!(info.kitty_listen_on, "unix:/tmp/kitty");
    }

    #[test]
    fn test_resolve_terminal_info_falls_back_for_legacy_kitty_metadata() {
        let info = resolve_terminal_info(
            None,
            Some(r#"{"pane_id":"pane-1","kitty_listen_on":"unix:/tmp/kitty"}"#),
        );
        assert_eq!(info.preset_name, "kitty-split");
        assert_eq!(info.pane_id, "pane-1");
        assert_eq!(info.kitty_listen_on, "unix:/tmp/kitty");
    }

    #[test]
    fn test_is_zellij_preset_does_not_match_name_prefix_only() {
        assert!(!is_zellij_preset("zellijish"));
    }

    #[test]
    fn test_wezterm_open_argv_selects_powershell_on_windows() {
        let merged = crate::config::get_merged_preset("wezterm").unwrap();
        let win = merged.open_argv(true);
        assert!(win.iter().any(|a| a == "powershell"));
        assert!(win.iter().any(|a| a == "-File"));
        assert!(!win.iter().any(|a| a == "bash"));
        let unix = merged.open_argv(false);
        assert!(unix.iter().any(|a| a == "bash"));
    }

    #[test]
    fn test_mintty_open_argv_has_no_bash() {
        let merged = crate::config::get_merged_preset("mintty").unwrap();
        let argv = merged.open_argv(true);
        assert_eq!(argv.first().map(String::as_str), Some("mintty"));
        assert!(
            !argv.iter().any(|a| a == "bash"),
            "mintty must not hand a .ps1 to bash"
        );
        assert!(argv.iter().any(|a| a == "powershell"));
    }

    #[test]
    fn test_warp_preset_registered() {
        let preset = crate::shared::terminal_presets::get_terminal_preset("warp").unwrap();
        assert_eq!(preset.app_name, Some("Warp"));
        assert_eq!(preset.binary, None);
        let open = preset.open.select(false).unwrap();
        assert!(open.contains(&"warp://launch/hcom-{process_id}"));
        assert_eq!(preset.platforms, &["Darwin"]);
    }

    #[test]
    fn test_waveterm_preset_uses_run_separator() {
        let cmd = resolve_terminal_open_argv("waveterm").unwrap();
        let out = super::super::substitute_open_argv(
            &cmd,
            super::super::TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                ..super::super::TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(out, vec!["wsh", "run", "--", "bash", "/tmp/test.sh"]);
    }

    #[test]
    fn test_herdr_preset_template_uses_stable_instance_name() {
        let preset = crate::shared::terminal_presets::get_terminal_preset("herdr").unwrap();
        let open = preset.open.select(false).unwrap();
        assert!(open.contains(&"{script}"));
        assert!(open.contains(&"{instance_name}"));
        assert!(
            !open.contains(&"{pane_title}"),
            "herdr preset must not use {{pane_title}} as agent name"
        );
        assert!(open.contains(&"{cwd}"));
        assert!(!open.contains(&"{process_id}"));
        assert_eq!(preset.binary, Some("herdr"));
        let close = preset.close.select(false).unwrap();
        assert!(close.contains(&"{pane_id}"));
    }

    #[test]
    fn test_get_available_presets_always_has_default_and_custom() {
        let presets = get_available_presets();
        assert_eq!(presets.first().unwrap().0, "default");
        assert_eq!(presets.last().unwrap().0, "custom");
    }

    #[test]
    fn terminal_preset_platform_capability() {
        use crate::config::terminal_preset_supported_on;
        assert!(terminal_preset_supported_on("iterm", "Darwin"));
        assert!(!terminal_preset_supported_on("iterm", "Windows"));
        assert!(terminal_preset_supported_on("wttab", "Windows"));
        assert!(!terminal_preset_supported_on("wttab", "Darwin"));
        assert!(terminal_preset_supported_on("wezterm", "Windows"));
        assert!(!terminal_preset_supported_on("nope", "Darwin"));
    }
}
