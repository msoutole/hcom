//! Windows-specific terminal helpers (cmd/.bat launchers, bash-to-PowerShell rewrites).

use std::path::Path;
use std::process::Command;

use super::which_bin;
use anyhow::{Result, bail};

pub fn executable_command(name: &str) -> Command {
    let resolved = which_bin(name).unwrap_or_else(|| name.to_string());
    #[cfg(windows)]
    if Path::new(&resolved)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
    {
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/c"]).arg(resolved);
        return command;
    }
    Command::new(resolved)
}

/// Bypass npm's Windows `codex.cmd` shim when launching the interactive tool.
///
/// The shim expands `%*` through cmd.exe, which corrupts quote-bearing config
/// values such as the multiline `developer_instructions` bootstrap. Invoking
/// the package's Node entrypoint directly preserves Rust's argv boundaries.
#[cfg(windows)]
pub fn resolve_windows_tool_launcher(tool: &str, resolved: &str) -> Option<(String, Vec<String>)> {
    if !Path::new(resolved)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd"))
    {
        return None;
    }
    let prefix = Path::new(resolved).parent()?;
    let node = which_bin("node")?;

    match tool {
        "codex" => {
            let entrypoint = prefix.join("node_modules/@openai/codex/bin/codex.js");
            if !entrypoint.is_file() {
                return None;
            }
            Some((node, vec![entrypoint.to_string_lossy().into_owned()]))
        }
        "opencode" => {
            // npm scoped: @opencode-ai/opencode
            let entrypoint = prefix.join("node_modules/@opencode-ai/opencode/bin/opencode.js");
            let entrypoint = if entrypoint.is_file() {
                entrypoint
            } else {
                // npm unscoped fallback
                let fallback = prefix.join("node_modules/opencode/bin/opencode.js");
                if !fallback.is_file() {
                    return None;
                }
                fallback
            };
            Some((node, vec![entrypoint.to_string_lossy().into_owned()]))
        }
        _ => None,
    }
}

/// Resolve the `bash` command to run on Unix, preferring a `PATH` match and
/// falling back to `/bin/bash`. Shared by the background and run-here launch
/// paths so they can't drift on which `bash` gets invoked.
#[cfg(any(windows, test))]
fn is_bash_interp(tok: &str) -> bool {
    std::path::Path::new(tok)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("bash"))
}

/// Detect an unrunnable non-adjacent bash-family `{script}` custom template: a
/// bash-family interpreter token (`bash`, `bash.exe`, `/bin/bash`, …) with a
/// LATER (non-adjacent) `{script}`, so the generated `.ps1` would be handed to
/// bash and can't be rewritten by a simple splice — e.g. `bash -c {script}`,
/// `bash -x {script}`, `bash.exe -i {script}`. Returns the offending
/// interpreter token. Adjacent `<interp> {script}` is handled by
/// `shellify_bash_script_pair` and is NOT flagged.
#[cfg(any(windows, test))]
fn unsupported_bash_script_interp(argv: &[String]) -> Option<String> {
    let interp = argv.iter().position(|a| is_bash_interp(a))?;
    let script = argv.iter().position(|a| a.contains("{script}"))?;
    if script <= interp + 1 {
        return None;
    }
    Some(argv[interp].clone())
}

/// Rewrite an adjacent bash-family `{script}` executor pair to PowerShell in a
/// custom (non-preset) command argv, so the generated `.ps1` runs on Windows
/// without requiring Git Bash on PATH. Matches an adjacent `<interp>` +
/// `{script}` pair anywhere in the argv — not just at argv[0] — where
/// `<interp>` is any bash-family token (`bash`, `bash.exe`, `/bin/bash`, …).
/// A bash-family token with a NON-adjacent `{script}` (e.g. `bash -c {script}`)
/// can't be rewritten by a simple splice — it is rejected with a clear error
/// instead of silently launching a broken command (see
/// `unsupported_bash_script_interp`).
///
/// Off Windows this is a passthrough (the generated script is a bash script).
#[cfg(not(windows))]
pub(crate) fn windows_shellify_custom_argv(argv: Vec<String>) -> Result<Vec<String>> {
    Ok(argv)
}

#[cfg(windows)]
pub(crate) fn windows_shellify_custom_argv(argv: Vec<String>) -> Result<Vec<String>> {
    if let Some(interp) = unsupported_bash_script_interp(&argv) {
        bail!(
            "custom terminal command runs `{interp}` with a non-adjacent `{{script}}` and \
             cannot run the generated PowerShell script on Windows; use `{interp} {{script}}` \
             (adjacent, no flags) or a native command"
        );
    }
    Ok(shellify_bash_script_pair(argv))
}

/// Replace an adjacent bash-family + `{script}` pair with the PowerShell `.ps1`
/// launcher. Platform-agnostic so it can be unit-tested on any host; the
/// `windows_shellify_custom_argv` wrapper only applies it on Windows.
#[cfg(any(windows, test))]
fn shellify_bash_script_pair(mut argv: Vec<String>) -> Vec<String> {
    if let Some(i) = argv
        .windows(2)
        .position(|w| is_bash_interp(&w[0]) && w[1] == "{script}")
    {
        argv.splice(
            i..i + 1,
            [
                "powershell".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-NoExit".to_string(),
                "-File".to_string(),
            ],
        );
    }
    argv
}

/// Resolve preset name to an open-command argv template (placeholders intact).
///
/// On macOS, if CLI binary isn't in PATH but .app bundle exists,
/// uses a hardcoded fallback or substitutes the full binary path. The returned
/// argv still contains placeholders like `{script}`; substitute via
/// `substitute_open_argv`.
pub(crate) fn windows_default_terminal_display_name(has_wt: bool) -> &'static str {
    if has_wt {
        "Windows Terminal"
    } else {
        "cmd.exe"
    }
}

/// Return a human-readable name for the platform's built-in fallback terminal
/// (used when `terminal = "default"` and no terminal is detected from env).
fn windows_default_terminal_template(has_wt: bool) -> Vec<String> {
    let to_vec = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    if has_wt {
        return to_vec(&[
            "wt",
            "--",
            "powershell",
            "-ExecutionPolicy",
            "Bypass",
            "-NoExit",
            "-File",
            "{script}",
        ]);
    }
    to_vec(&[
        "cmd",
        "/c",
        "start",
        "",
        "powershell",
        "-ExecutionPolicy",
        "Bypass",
        "-NoExit",
        "-File",
        "{script}",
    ])
}

/// Default Windows terminal launch argv ({script} substituted by the caller).
pub(crate) fn get_windows_terminal_argv() -> Vec<String> {
    windows_default_terminal_template(which_bin("wt").is_some())
}

/// Spawn terminal process, detached when inside AI tools.
///
/// Returns (success, stdout_first_line) — stdout captured for {id} in close commands.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn shellify(argv: &[&str]) -> Vec<String> {
        shellify_bash_script_pair(argv.iter().map(|s| s.to_string()).collect())
    }

    const PS: &[&str] = &[
        "powershell",
        "-ExecutionPolicy",
        "Bypass",
        "-NoExit",
        "-File",
        "{script}",
    ];

    #[test]
    fn shellify_rewrites_leading_bash_script() {
        assert_eq!(shellify(&["bash", "{script}"]), PS);
    }

    #[test]
    fn shellify_rewrites_non_leading_bash_script() {
        let mut expected = vec!["myterm".to_string(), "--".to_string()];
        expected.extend(PS.iter().map(|s| s.to_string()));
        assert_eq!(shellify(&["myterm", "--", "bash", "{script}"]), expected);
        let mut expected = vec!["gnome-terminal".to_string(), "--".to_string()];
        expected.extend(PS.iter().map(|s| s.to_string()));
        assert_eq!(
            shellify(&["gnome-terminal", "--", "bash", "{script}"]),
            expected
        );
    }

    #[test]
    fn shellify_rewrites_bash_family_interpreters() {
        assert_eq!(shellify(&["bash.exe", "{script}"]), PS);
        assert_eq!(shellify(&["/bin/bash", "{script}"]), PS);
    }

    #[test]
    fn shellify_leaves_bash_with_flags_alone() {
        assert_eq!(
            shellify(&["bash", "-c", "{script}"]),
            vec!["bash", "-c", "{script}"]
        );
    }

    #[test]
    fn shellify_leaves_non_bash_alone() {
        assert_eq!(
            shellify(&["myterm", "-e", "{script}"]),
            vec!["myterm", "-e", "{script}"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_codex_npm_launcher_bypasses_cmd_shim() {
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("codex.cmd");
        let entrypoint = temp
            .path()
            .join("node_modules")
            .join("@openai")
            .join("codex")
            .join("bin")
            .join("codex.js");
        std::fs::create_dir_all(entrypoint.parent().unwrap()).unwrap();
        std::fs::write(&shim, "@echo off\r\n").unwrap();
        std::fs::write(&entrypoint, "").unwrap();

        let (launcher, args) =
            resolve_windows_tool_launcher("codex", shim.to_str().unwrap()).unwrap();
        assert!(
            Path::new(&launcher)
                .file_stem()
                .is_some_and(|stem| stem.eq_ignore_ascii_case("node"))
        );
        let expected = entrypoint.to_string_lossy().replace('/', "\\");
        let actual = args[0].replace('/', "\\");
        assert_eq!(actual, expected);
        assert!(resolve_windows_tool_launcher("claude", shim.to_str().unwrap()).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_opencode_npm_launcher_bypasses_cmd_shim() {
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("opencode.cmd");
        let entrypoint = temp
            .path()
            .join("node_modules")
            .join("@opencode-ai")
            .join("opencode")
            .join("bin")
            .join("opencode.js");
        std::fs::create_dir_all(entrypoint.parent().unwrap()).unwrap();
        std::fs::write(&shim, "@echo off\r\n").unwrap();
        std::fs::write(&entrypoint, "").unwrap();

        let (launcher, args) =
            resolve_windows_tool_launcher("opencode", shim.to_str().unwrap()).unwrap();
        assert!(
            Path::new(&launcher)
                .file_stem()
                .is_some_and(|stem| stem.eq_ignore_ascii_case("node"))
        );
        let expected = entrypoint.to_string_lossy().replace('/', "\\");
        let actual = args[0].replace('/', "\\");
        assert_eq!(actual, expected);
        assert!(resolve_windows_tool_launcher("claude", shim.to_str().unwrap()).is_none());
    }

    #[test]
    fn windows_cmd_fallback_leaves_spaced_script_unquoted() {
        let tmpl = windows_default_terminal_template(false);
        let script = r"C:\Users\a b\hcom\s.ps1";
        let out: Vec<String> = tmpl.iter().map(|a| a.replace("{script}", script)).collect();
        assert_eq!(out.last().unwrap(), script);
        assert!(!out.last().unwrap().contains('"'));
        assert_eq!(out[3], "");
    }

    #[test]
    fn windows_status_name_tracks_launch_planner() {
        assert_eq!(windows_default_terminal_template(true)[0], "wt");
        assert_eq!(
            windows_default_terminal_display_name(true),
            "Windows Terminal"
        );
        assert_eq!(windows_default_terminal_template(false)[0], "cmd");
        assert_eq!(windows_default_terminal_display_name(false), "cmd.exe");
    }

    #[test]
    fn detects_unsupported_bash_c_script() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            unsupported_bash_script_interp(&v(&["bash", "-c", "{script}"])).as_deref(),
            Some("bash")
        );
        assert!(unsupported_bash_script_interp(&v(&["bash", "-x", "{script}"])).is_some());
        assert!(unsupported_bash_script_interp(&v(&["bash", "-i", "{script}"])).is_some());
        assert!(unsupported_bash_script_interp(&v(&["bash", "-lc", "{script}"])).is_some());
        assert_eq!(
            unsupported_bash_script_interp(&v(&["bash.exe", "-c", "{script}"])).as_deref(),
            Some("bash.exe")
        );
        assert!(unsupported_bash_script_interp(&v(&["/bin/bash", "-c", "{script}"])).is_some());
        assert!(unsupported_bash_script_interp(&v(&["bash", "{script}"])).is_none());
        assert!(unsupported_bash_script_interp(&v(&["bash.exe", "{script}"])).is_none());
        assert!(unsupported_bash_script_interp(&v(&["/bin/bash", "{script}"])).is_none());
        assert!(
            unsupported_bash_script_interp(&v(&["gnome-terminal", "--", "bash", "{script}"]))
                .is_none()
        );
        assert!(unsupported_bash_script_interp(&v(&["mypowershell", "{script}"])).is_none());
    }
}
