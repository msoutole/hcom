//! Bash/PowerShell script creation for terminal launches.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use super::{ps_quote, resolve_termux_tool_launcher, shell_quote, which_bin};
use crate::shared::constants::HCOM_IDENTITY_VARS;
use crate::shared::tool_detection::tool_marker_vars;

pub fn build_env_string(env_vars: &HashMap<String, String>, format_type: &str) -> String {
    let mut valid: Vec<(&String, &String)> = env_vars
        .iter()
        .filter(|(k, _)| {
            k.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .collect();
    valid.sort_by_key(|(k, _)| k.to_string());

    if format_type == "bash_export" {
        valid
            .iter()
            .map(|(k, v)| format!("export {}={};", k, shell_quote(v)))
            .collect::<Vec<_>>()
            .join(" ")
    } else if format_type == "powershell" {
        valid
            .iter()
            .map(|(k, v)| format!("$env:{} = {}", k, ps_quote(v)))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        valid
            .iter()
            .map(|(k, v)| format!("{}={}", k, shell_quote(v)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Shell-quote a string for bash.
fn launch_display_name<'a>(command_str: &str, tool_name: Option<&'a str>) -> &'a str {
    tool_name.unwrap_or_else(|| {
        let cmd_lower = command_str.to_lowercase();
        if cmd_lower.contains("opencode") {
            "OpenCode"
        } else if cmd_lower.contains("kilo") {
            "Kilo Code"
        } else if cmd_lower.contains("cursor-agent") {
            "Cursor Agent"
        } else if cmd_lower.contains("gemini") {
            "Gemini"
        } else if cmd_lower.contains("codex") {
            "Codex"
        } else if cmd_lower.contains("claude") {
            "Claude Code"
        } else {
            "hcom"
        }
    })
}

/// Create a bash script for terminal launch.
///
/// Scripts provide uniform execution across all platforms/terminals.
pub fn create_bash_script(
    script_file: &Path,
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    command_str: &str,
    background: bool,
    tool_name: Option<&str>,
    opens_new_window: bool,
) -> Result<()> {
    let tool_name = launch_display_name(command_str, tool_name);

    let mut f = fs::File::create(script_file).context("Failed to create script file")?;

    writeln!(f, "#!/bin/bash")?;
    writeln!(f, "printf \"\\033]0;hcom: starting {}...\\007\"", tool_name)?;
    writeln!(f, "echo \"Starting {}...\"", tool_name)?;

    // Unset tool markers and identity vars to prevent inheritance
    writeln!(f, "unset {}", tool_marker_vars().join(" "))?;
    writeln!(f, "unset {}", HCOM_IDENTITY_VARS.join(" "))?;

    // Discover paths for minimal environments (kitty splits, etc.)
    let mut paths_to_add: Vec<String> = Vec::new();

    fn add_path(paths: &mut Vec<String>, binary_path: Option<String>) {
        if let Some(bp) = binary_path
            && let Some(dir) = Path::new(&bp).parent()
        {
            let dir_str = dir.to_string_lossy().to_string();
            if !paths.contains(&dir_str) {
                paths.push(dir_str);
            }
        }
    }

    // Always add hcom's own directory
    add_path(&mut paths_to_add, which_bin("hcom"));
    // Add python3 to PATH for agents that need it
    add_path(&mut paths_to_add, which_bin("python3"));
    // Detect tool from command and add its path
    let cmd_stripped = command_str.trim_start();
    let tool_cmd = cmd_stripped.split_whitespace().next().unwrap_or("");
    add_path(&mut paths_to_add, which_bin(tool_cmd));
    // Claude needs node
    if tool_cmd == "claude" {
        add_path(&mut paths_to_add, which_bin("node"));
    }

    if !paths_to_add.is_empty() {
        writeln!(f, "export PATH=\"{}:$PATH\"", paths_to_add.join(":"))?;
    }

    // Write env exports
    let env_str = build_env_string(env, "bash_export");
    if !env_str.is_empty() {
        writeln!(f, "{}", env_str)?;
    }

    if let Some(dir) = cwd {
        writeln!(f, "cd {}", shell_quote(dir))?;
    }

    // Resolve tool path for full path execution.
    // On Termux, npm-installed tools have shebangs like #!/usr/bin/env node which
    // fail (no /usr/bin/env). Detect node shebangs and rewrite to: node /path/to/tool args
    let mut final_command = command_str.to_string();
    if !tool_cmd.is_empty()
        && let Some(tool_path) = which_bin(tool_cmd)
    {
        if let Some((launcher, prefix_args)) = resolve_termux_tool_launcher(tool_cmd, &tool_path) {
            let mut replacement_parts = vec![shell_quote(&launcher)];
            replacement_parts.extend(prefix_args.iter().map(|arg| shell_quote(arg)));
            final_command = final_command.replacen(
                &format!("{} ", tool_cmd),
                &format!("{} ", replacement_parts.join(" ")),
                1,
            );
        } else {
            final_command = final_command.replacen(
                &format!("{} ", tool_cmd),
                &format!("{} ", shell_quote(&tool_path)),
                1,
            );
        }
    }

    writeln!(f, "{}", final_command)?;

    if opens_new_window {
        // Clear hcom state from the interactive shell left open after the tool
        // exits. Derive from HCOM_IDENTITY_VARS (so new identity/batch vars are
        // covered automatically) plus the non-identity per-launch vars exported
        // above that aren't in that list.
        let mut leftover_vars: Vec<&str> = HCOM_IDENTITY_VARS.to_vec();
        leftover_vars.extend(["HCOM_TAG", "HCOM_CODEX_SANDBOX_MODE"]);
        writeln!(f, "unset {}", leftover_vars.join(" "))?;
        writeln!(f, "rm -f {}", shell_quote(&script_file.to_string_lossy()))?;
        writeln!(f, "exec bash -l")?;
    } else if !background {
        writeln!(f, "hcom_status=$?")?;
        writeln!(f, "rm -f {}", shell_quote(&script_file.to_string_lossy()))?;
        writeln!(f, "exit $hcom_status")?;
    }

    // Make executable
    crate::sys::fs::set_executable(script_file)?;

    Ok(())
}

/// Quote a string as a PowerShell single-quoted literal (embedded `'` doubled).
fn ps_env_assignments(env_vars: &HashMap<String, String>) -> Vec<String> {
    let mut valid: Vec<(&String, &String)> = env_vars
        .iter()
        .filter(|(k, _)| {
            k.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .collect();
    valid.sort_by_key(|(k, _)| k.to_string());
    valid
        .iter()
        .map(|(k, v)| format!("$env:{} = {}", k, ps_quote(v)))
        .collect()
}

/// Create a PowerShell launch script — the Windows-native equivalent of
/// `create_bash_script`. Emits a `.ps1` that sets the window title, scrubs
/// inherited tool/identity vars, prepends discovered tool directories to PATH,
/// assigns the per-launch env, changes directory, then runs the tool command
/// via the call operator. The window-open vs. run-once cleanup mirrors the
/// bash version; the window is kept alive by launching with `powershell -NoExit`.
pub fn create_powershell_script(
    script_file: &Path,
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    command_str: &str,
    background: bool,
    tool_name: Option<&str>,
    opens_new_window: bool,
) -> Result<()> {
    let tool_name = launch_display_name(command_str, tool_name);

    let mut f = fs::File::create(script_file).context("Failed to create script file")?;
    // Windows PowerShell 5.1 decodes BOM-less scripts using the active ANSI
    // code page. Force UTF-8 so non-ASCII paths, prompts, and environment
    // values survive on stock Windows PowerShell.
    f.write_all(&[0xEF, 0xBB, 0xBF])?;

    writeln!(
        f,
        "$Host.UI.RawUI.WindowTitle = \"hcom: starting {}...\"",
        tool_name
    )?;
    writeln!(f, "Write-Host \"Starting {}...\"", tool_name)?;

    // Scrub inherited tool markers and identity vars so the child can't inherit
    // them (PowerShell ignores Env: entries that don't exist).
    let scrub: Vec<String> = tool_marker_vars()
        .iter()
        .chain(HCOM_IDENTITY_VARS.iter())
        .map(|v| format!("Env:{v}"))
        .collect();
    writeln!(
        f,
        "Remove-Item {} -ErrorAction SilentlyContinue",
        scrub.join(",")
    )?;

    // Discover paths for minimal environments.
    let mut paths_to_add: Vec<String> = Vec::new();

    fn add_path(paths: &mut Vec<String>, binary_path: Option<String>) {
        if let Some(bp) = binary_path
            && let Some(dir) = Path::new(&bp).parent()
        {
            let dir_str = dir.to_string_lossy().to_string();
            if !paths.contains(&dir_str) {
                paths.push(dir_str);
            }
        }
    }

    add_path(&mut paths_to_add, which_bin("hcom"));
    add_path(&mut paths_to_add, which_bin("python3"));
    let cmd_stripped = command_str.trim_start();
    let tool_cmd = cmd_stripped.split_whitespace().next().unwrap_or("");
    add_path(&mut paths_to_add, which_bin(tool_cmd));
    if tool_cmd == "claude" {
        add_path(&mut paths_to_add, which_bin("node"));
    }

    if !paths_to_add.is_empty() {
        // Windows PATH is `;`-separated.
        let prefix = format!("{};", paths_to_add.join(";"));
        writeln!(f, "$env:PATH = {} + $env:PATH", ps_quote(&prefix))?;
    }

    for line in ps_env_assignments(env) {
        writeln!(f, "{line}")?;
    }

    if let Some(dir) = cwd {
        writeln!(f, "Set-Location {}", ps_quote(dir))?;
    }

    // Resolve the tool to a full path and invoke it through the call operator so
    // a quoted path runs as a command. If the tool isn't found, fall through to
    // the bare command name (resolved via the PATH we just prepended).
    let mut final_command = command_str.to_string();
    if !tool_cmd.is_empty()
        && let Some(tool_path) = which_bin(tool_cmd)
    {
        let replaced = final_command.replacen(
            &format!("{tool_cmd} "),
            &format!("& {} ", ps_quote(&tool_path)),
            1,
        );
        final_command = if replaced != final_command {
            replaced
        } else {
            // No arguments: the command is exactly the tool name (no trailing
            // space to match), so replace the bare name directly.
            final_command.replacen(tool_cmd, &format!("& {}", ps_quote(&tool_path)), 1)
        };
    }

    writeln!(f, "{final_command}")?;

    if opens_new_window {
        // Clear hcom state from the interactive shell left open after the tool
        // exits (window persists via `powershell -NoExit`).
        let mut leftover_vars: Vec<&str> = HCOM_IDENTITY_VARS.to_vec();
        leftover_vars.extend(["HCOM_TAG", "HCOM_CODEX_SANDBOX_MODE"]);
        let leftover: Vec<String> = leftover_vars.iter().map(|v| format!("Env:{v}")).collect();
        writeln!(
            f,
            "Remove-Item {} -ErrorAction SilentlyContinue",
            leftover.join(",")
        )?;
        writeln!(
            f,
            "Remove-Item -Force -ErrorAction SilentlyContinue {}",
            ps_quote(&script_file.to_string_lossy())
        )?;
    } else if !background {
        writeln!(f, "$hcom_status = $LASTEXITCODE")?;
        writeln!(
            f,
            "Remove-Item -Force -ErrorAction SilentlyContinue {}",
            ps_quote(&script_file.to_string_lossy())
        )?;
        writeln!(f, "exit $hcom_status")?;
    }

    Ok(())
}

/// Build clean env for terminal launcher subprocesses.
///
/// Strips AI tool markers, hcom identity vars, and terminal context vars.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_env_string_powershell_format() {
        let mut env = HashMap::new();
        env.insert("HCOM_A".to_string(), "x".to_string());
        env.insert("HCOM_B".to_string(), "y'z".to_string());
        let out = build_env_string(&env, "powershell");
        assert_eq!(out, "$env:HCOM_A = 'x'\n$env:HCOM_B = 'y''z'");
    }

    #[test]
    fn test_build_env_string_bash() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let result = build_env_string(&env, "bash");
        assert_eq!(result, "FOO=bar");
    }

    #[test]
    fn test_build_env_string_export() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar baz".to_string());
        let result = build_env_string(&env, "bash_export");
        assert_eq!(result, "export FOO='bar baz';");
    }

    #[test]
    fn test_build_env_string_filters_invalid() {
        let mut env = HashMap::new();
        env.insert("GOOD".to_string(), "val".to_string());
        env.insert("123BAD".to_string(), "val".to_string());
        let result = build_env_string(&env, "bash");
        assert!(result.contains("GOOD"));
        assert!(!result.contains("123BAD"));
    }

    #[test]
    fn test_ps_env_assignments_sorted_and_validated() {
        let mut env = HashMap::new();
        env.insert("ZED".to_string(), "z".to_string());
        env.insert("ABE".to_string(), "a'b".to_string());
        env.insert("1bad".to_string(), "skip".to_string());
        let lines = ps_env_assignments(&env);
        assert_eq!(
            lines,
            vec![
                "$env:ABE = 'a''b'".to_string(),
                "$env:ZED = 'z'".to_string(),
            ]
        );
    }

    #[test]
    fn test_create_powershell_script_window_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("launch.ps1");
        let mut env = HashMap::new();
        env.insert("HCOM_TOOL".to_string(), "claude".to_string());
        create_powershell_script(
            &script,
            &env,
            Some("/work/dir"),
            "claude --foo",
            false,
            None,
            true,
        )
        .unwrap();
        assert!(
            std::fs::read(&script)
                .unwrap()
                .starts_with(&[0xEF, 0xBB, 0xBF])
        );
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("$Host.UI.RawUI.WindowTitle = \"hcom: starting Claude Code...\""));
        assert!(content.contains("Write-Host \"Starting Claude Code...\""));
        assert!(content.contains("Remove-Item Env:"));
        assert!(content.contains("$env:HCOM_TOOL = 'claude'"));
        assert!(content.contains("Set-Location '/work/dir'"));
        assert!(content.contains("--foo"));
        assert!(content.contains("Remove-Item -Force -ErrorAction SilentlyContinue"));
        assert!(!content.contains("exit $hcom_status"));
    }

    #[test]
    fn test_create_powershell_script_run_once_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("launch.ps1");
        let env = HashMap::new();
        create_powershell_script(&script, &env, None, "codex", false, None, false).unwrap();
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("$hcom_status = $LASTEXITCODE"));
        assert!(content.contains("exit $hcom_status"));
        assert!(!content.contains("Set-Location"));
    }
}
