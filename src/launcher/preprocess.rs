use std::path::Path;

use anyhow::{Result, bail};

use crate::launcher::LaunchTool;
use crate::tools::launch_arg_validation::{
    ANTIGRAVITY_REJECTED_ARGS, GEMINI_REJECTED_ARGS, KILO_REJECTED_ARGS, KIMI_REJECTED_ARGS,
    OMP_REJECTED_ARGS, OPENCODE_REJECTED_ARGS, PI_REJECTED_ARGS, validate_rejected_args,
};

/// Inject ephemeral workspace trust flags into args for gemini and codex.
///
/// - gemini: adds `--skip-trust` (session-scoped, no persisted state)
/// - codex: adds `-c` with a TOML inline-table value that sets trust for the
///   canonical CWD. Uses key `projects` (no dots in the key path) so codex's
///   naive `.`-splitting never touches the path itself, making it robust to
///   dotted directory components. The path is TOML-escaped (`\` and `"`) —
///   every Windows path carries backslashes, and unescaped they made codex
///   reject the value as "invalid type: string, expected a map".
///
/// When `auto_trust` is false, returns immediately without modifying args.
/// Idempotent: no-op if the relevant flag is already present.
/// Cursor is handled separately via `ensure_cursor_workspace_trusted`.
pub(crate) fn inject_workspace_trust_args(
    tool: &LaunchTool,
    canonical_dir: &Path,
    args: &mut Vec<String>,
    auto_trust: bool,
) {
    if !auto_trust {
        return;
    }
    match tool {
        LaunchTool::Gemini if !args.iter().any(|a| a == "--skip-trust") => {
            args.push("--skip-trust".to_string());
        }
        LaunchTool::Gemini => {}
        LaunchTool::Codex => {
            let already_set = args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("trust_level"));
            if !already_set {
                let canonical_str = canonical_dir.to_string_lossy();
                // std::fs::canonicalize returns \\?\-prefixed verbatim paths on
                // Windows; codex tracks projects by their ordinary absolute
                // form, so strip the prefix or the trust key never matches.
                let simplified = if let Some(unc) = canonical_str.strip_prefix(r"\\?\UNC\") {
                    format!(r"\\{unc}")
                } else {
                    canonical_str
                        .strip_prefix(r"\\?\")
                        .unwrap_or(&canonical_str)
                        .to_string()
                };
                // codex -c: key="projects" (no dots → no split issue), value is a TOML
                // inline table with the quoted path as key. apply_single_override replaces
                // the projects table for this session only (file stays untouched).
                // Escape backslashes before quotes: the quote escape introduces
                // a backslash that must not itself be doubled.
                let toml_escaped = crate::runtime_env::toml_escape_path(&simplified);
                let trust_override = format!(
                    "projects={{ \"{}\" = {{ trust_level = \"trusted\" }} }}",
                    toml_escaped
                );
                args.push("-c".to_string());
                args.push(trust_override);
            }
        }
        _ => {}
    }
}

pub(super) fn validate_launch_count(tool: &LaunchTool, count: usize) -> Result<()> {
    if count == 0 {
        bail!("Count must be positive");
    }
    let max = tool.spec().launch.max_launch_count;
    if count > max {
        bail!(
            "Too many {} instances requested (max {})",
            tool.as_str(),
            max
        );
    }
    Ok(())
}

pub(super) fn inject_omp_extension_args(tool: &LaunchTool, args: &mut Vec<String>) {
    if !matches!(tool, LaunchTool::Omp) {
        return;
    }
    let extension_args = crate::hooks::omp::extension_inject_args();
    let plugin_path = extension_args
        .get(1)
        .map(String::as_str)
        .unwrap_or_default();
    let already_present = args
        .windows(2)
        .any(|window| matches!(window, [flag, path] if flag == "-e" && path == plugin_path));
    if !already_present {
        args.extend(extension_args);
    }
}

pub(super) fn append_initial_prompt_args(
    tool: &LaunchTool,
    args: &mut Vec<String>,
    prompt: String,
) -> Result<()> {
    match tool.spec().launch.initial_prompt {
        crate::integration_spec::InitialPromptShape::Unsupported { reason } => bail!("{reason}"),
        crate::integration_spec::InitialPromptShape::DashDashPositional => {
            args.push("--".to_string());
            args.push(prompt);
        }
        crate::integration_spec::InitialPromptShape::Positional => args.push(prompt),
        crate::integration_spec::InitialPromptShape::Flag(flag) => {
            args.push(flag.to_string());
            args.push(prompt);
        }
    }
    Ok(())
}

/// Validate tool args (pure parsing, no mutation).
pub(crate) fn validate_tool_args(tool: &LaunchTool, args: &[String]) -> Vec<String> {
    match tool {
        LaunchTool::Claude | LaunchTool::ClaudePty | LaunchTool::Codex => Vec::new(),
        LaunchTool::Gemini => {
            validate_rejected_args("Gemini", "hcom gemini", args, GEMINI_REJECTED_ARGS)
        }
        LaunchTool::Cursor => crate::tools::cursor_preprocessing::validate_cursor_args(args),
        LaunchTool::Kimi => validate_rejected_args("Kimi", "hcom kimi", args, KIMI_REJECTED_ARGS),
        LaunchTool::OpenCode => {
            validate_rejected_args("OpenCode", "hcom opencode", args, OPENCODE_REJECTED_ARGS)
        }
        LaunchTool::Kilo => validate_rejected_args("Kilo", "hcom kilo", args, KILO_REJECTED_ARGS),
        LaunchTool::Pi => validate_rejected_args("Pi", "hcom pi", args, PI_REJECTED_ARGS),
        LaunchTool::Omp => validate_rejected_args("Oh My Pi", "hcom omp", args, OMP_REJECTED_ARGS),
        LaunchTool::Antigravity => validate_rejected_args(
            "Antigravity",
            "hcom antigravity",
            args,
            ANTIGRAVITY_REJECTED_ARGS,
        ),
        LaunchTool::Copilot => crate::tools::copilot_preprocessing::validate_copilot_args(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_count_uses_per_tool_spec_limit() {
        assert!(validate_launch_count(&LaunchTool::Kimi, 10).is_ok());
        let err = validate_launch_count(&LaunchTool::Kimi, 11).unwrap_err();
        assert!(err.to_string().contains("max 10"));

        assert!(validate_launch_count(&LaunchTool::Claude, 100).is_ok());
        let err = validate_launch_count(&LaunchTool::Claude, 101).unwrap_err();
        assert!(err.to_string().contains("max 100"));
    }

    #[test]
    fn unsupported_initial_prompt_is_spec_driven() {
        let mut args = Vec::new();
        let err =
            append_initial_prompt_args(&LaunchTool::Kimi, &mut args, "task".into()).unwrap_err();
        assert!(
            err.to_string()
                .contains("kimi does not support an initial prompt")
        );
        assert!(args.is_empty());

        append_initial_prompt_args(&LaunchTool::Gemini, &mut args, "task".into()).unwrap();
        assert_eq!(args, vec!["task"]);
    }

    #[test]
    fn initial_prompt_flag_shape_appends_after_native_prompt() {
        let mut args = vec!["--prompt".to_string(), "native prompt".to_string()];
        append_initial_prompt_args(&LaunchTool::OpenCode, &mut args, "hcom prompt".into()).unwrap();
        assert_eq!(
            args,
            vec!["--prompt", "native prompt", "--prompt", "hcom prompt"]
        );
    }

    #[test]
    fn initial_prompt_positional_shape_appends_after_native_prompt() {
        let mut args = vec!["native prompt".to_string()];
        append_initial_prompt_args(&LaunchTool::Gemini, &mut args, "hcom prompt".into()).unwrap();
        assert_eq!(args, vec!["native prompt", "hcom prompt"]);
    }

    #[test]
    fn initial_prompt_dash_dash_shape_appends_after_native_prompt() {
        let mut args = vec!["--".to_string(), "native prompt".to_string()];
        append_initial_prompt_args(&LaunchTool::Claude, &mut args, "hcom prompt".into()).unwrap();
        assert_eq!(args, vec!["--", "native prompt", "--", "hcom prompt"]);
    }

    #[test]
    fn hcom_prompt_alone_is_injected_for_every_shape() {
        for (tool, expected) in [
            (
                LaunchTool::OpenCode,
                vec!["--prompt".to_string(), "hcom".to_string()],
            ),
            (LaunchTool::Gemini, vec!["hcom".to_string()]),
            (
                LaunchTool::Claude,
                vec!["--".to_string(), "hcom".to_string()],
            ),
        ] {
            let mut args = Vec::new();
            append_initial_prompt_args(&tool, &mut args, "hcom".into()).unwrap();
            assert_eq!(args, expected);
        }
    }

    #[test]
    fn positional_shape_does_not_treat_model_value_as_prompt() {
        let mut args = vec!["--model".to_string(), "safe-model".to_string()];
        append_initial_prompt_args(&LaunchTool::Gemini, &mut args, "hcom".into()).unwrap();
        assert_eq!(args.last().map(String::as_str), Some("hcom"));
    }

    #[test]
    fn validate_cursor_print_mode_fails_fast() {
        let errors = validate_tool_args(&LaunchTool::Cursor, &["--print".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("not supported"));
    }

    #[test]
    fn validate_kimi_rejects_prompt_mode_but_allows_resume() {
        let errors = validate_tool_args(&LaunchTool::Kimi, &["--prompt".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--prompt"));
        assert!(validate_tool_args(&LaunchTool::Kimi, &["--session".to_string()]).is_empty());
    }

    #[test]
    fn validate_opencode_rejects_run_but_allows_session() {
        let errors = validate_tool_args(&LaunchTool::OpenCode, &["run".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("run"));
        assert!(validate_tool_args(&LaunchTool::OpenCode, &["--session".to_string()]).is_empty());
        assert!(validate_tool_args(&LaunchTool::OpenCode, &["--prompt".to_string()]).is_empty());
    }

    #[test]
    fn validate_kilo_rejects_serve_but_allows_continue() {
        let errors = validate_tool_args(&LaunchTool::Kilo, &["serve".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("serve"));
        assert!(validate_tool_args(&LaunchTool::Kilo, &["--continue".to_string()]).is_empty());
    }

    #[test]
    fn validate_pi_rejects_print_but_allows_fork() {
        let errors = validate_tool_args(&LaunchTool::Pi, &["--print".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--print"));
        assert!(validate_tool_args(&LaunchTool::Pi, &["--fork".to_string()]).is_empty());
    }

    #[test]
    fn omp_extension_args_are_injected_once() {
        let mut args = vec!["--model".to_string(), "opus".to_string()];
        inject_omp_extension_args(&LaunchTool::Omp, &mut args);
        assert!(args.iter().any(|arg| arg == "-e"));
        assert!(args.iter().any(|arg| arg.ends_with("hcom.ts")));

        let once = args.clone();
        inject_omp_extension_args(&LaunchTool::Omp, &mut args);
        assert_eq!(args, once);
    }

    #[test]
    fn validate_antigravity_rejects_print_alias_but_allows_conversation() {
        let errors = validate_tool_args(&LaunchTool::Antigravity, &["--prompt".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--prompt"));
        assert!(
            validate_tool_args(&LaunchTool::Antigravity, &["--conversation".to_string()])
                .is_empty()
        );
        assert!(
            validate_tool_args(
                &LaunchTool::Antigravity,
                &["--prompt-interactive".to_string()]
            )
            .is_empty()
        );
    }

    // ── inject_workspace_trust_args ──────────────────────────────────────────

    #[test]
    fn test_auto_trust_workspace_default_true() {
        assert!(crate::config::HcomConfig::default().auto_trust_workspace);
    }

    #[test]
    fn test_gemini_flag_on_injects_skip_trust() {
        let dir = Path::new("/some/workspace");
        let mut args = vec!["--model".to_string(), "gemini-2.5-flash".to_string()];
        inject_workspace_trust_args(&LaunchTool::Gemini, dir, &mut args, true);
        assert!(args.contains(&"--skip-trust".to_string()));
    }

    #[test]
    fn test_gemini_flag_off_no_injection() {
        let dir = Path::new("/some/workspace");
        let mut args = vec!["--model".to_string(), "gemini-2.5-flash".to_string()];
        inject_workspace_trust_args(&LaunchTool::Gemini, dir, &mut args, false);
        assert!(!args.contains(&"--skip-trust".to_string()));
    }

    #[test]
    fn test_gemini_inject_idempotent_when_present() {
        let dir = Path::new("/some/workspace");
        let mut args = vec![
            "--skip-trust".to_string(),
            "--model".to_string(),
            "x".to_string(),
        ];
        inject_workspace_trust_args(&LaunchTool::Gemini, dir, &mut args, true);
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "--skip-trust").count(),
            1
        );
    }

    #[test]
    fn test_codex_flag_on_injects_trust_level() {
        let dir = Path::new("/my/project");
        let mut args = vec!["--model".to_string(), "o4-mini".to_string()];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        let c_idx = args
            .iter()
            .position(|a| a == "-c")
            .expect("-c not injected");
        let val = &args[c_idx + 1];
        assert!(val.contains("/my/project"), "path missing: {val}");
        assert!(val.contains("trust_level"), "trust_level missing: {val}");
        assert!(val.contains("\"trusted\""), "trusted value missing: {val}");
    }

    #[test]
    fn test_codex_flag_off_no_injection() {
        let dir = Path::new("/my/project");
        let mut args = vec!["--model".to_string(), "o4-mini".to_string()];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, false);
        assert!(!args.iter().any(|a| a == "-c"));
    }

    #[test]
    fn test_codex_inject_idempotent_when_present() {
        let dir = Path::new("/my/project");
        // Any -c value containing "trust_level" suppresses re-injection.
        let existing = r#"projects={ "/my/project" = { trust_level = "trusted" } }"#.to_string();
        let mut args = vec!["-c".to_string(), existing];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        assert_eq!(args.iter().filter(|a| a.as_str() == "-c").count(), 1);
    }

    #[test]
    fn test_codex_dotted_path_encoded_as_single_key() {
        // A path like /Users/x/proj.v2 has a dot in a component. The inline-table
        // format keeps the whole path as one TOML quoted key — no dot-splitting.
        let dir = Path::new("/Users/x/proj.v2");
        let mut args: Vec<String> = vec![];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        let c_idx = args
            .iter()
            .position(|a| a == "-c")
            .expect("-c not injected");
        let val = &args[c_idx + 1];
        // The full dotted path must survive intact as one quoted string.
        assert!(
            val.contains("\"/Users/x/proj.v2\""),
            "full dotted path must be a single quoted key: {val}"
        );
        assert!(val.contains("trust_level"), "trust_level missing: {val}");
    }

    #[test]
    fn test_codex_windows_verbatim_path_stripped_and_toml_escaped() {
        // std::fs::canonicalize yields \\?\-prefixed paths on Windows. The
        // prefix must go (codex keys projects by the plain absolute form) and
        // backslashes must be TOML-escaped or codex rejects the inline table.
        let dir = Path::new(r"\\?\C:\Users\x\proj");
        let mut args: Vec<String> = vec![];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        let val = &args[1];
        assert!(
            val.contains(r#""C:\\Users\\x\\proj""#),
            "path must be prefix-stripped and backslash-escaped: {val}"
        );
        assert!(
            !val.contains(r"\\?\"),
            "verbatim prefix must be stripped: {val}"
        );
    }

    #[test]
    fn test_codex_windows_verbatim_unc_path_keeps_server_form() {
        let dir = Path::new(r"\\?\UNC\server\share\dir");
        let mut args: Vec<String> = vec![];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        let val = &args[1];
        // \\?\UNC\server\... → \\server\... → TOML-escaped \\\\server\\...
        assert!(
            val.contains(r#""\\\\server\\share\\dir""#),
            "UNC path must keep its \\\\server form, escaped: {val}"
        );
    }

    #[test]
    fn test_non_trust_tools_unaffected() {
        let dir = Path::new("/some/workspace");
        for tool in &[
            LaunchTool::Claude,
            LaunchTool::ClaudePty,
            LaunchTool::OpenCode,
        ] {
            let mut args = vec!["--model".to_string(), "x".to_string()];
            inject_workspace_trust_args(tool, dir, &mut args, true);
            assert_eq!(
                args,
                vec!["--model".to_string(), "x".to_string()],
                "{tool:?} args should be unchanged"
            );
        }
    }
}
