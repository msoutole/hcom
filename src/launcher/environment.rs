use std::collections::{HashMap, HashSet};
use std::fs;

use crate::config::{self, HcomConfig};
use crate::launcher::{LaunchEnvRegime, LaunchTool};
use crate::paths;
use crate::shared::constants::HCOM_IDENTITY_VARS;
use crate::shared::tool_detection::tool_marker_vars;

/// Predict if launch will block current terminal (run in same window).
/// Find tool executable path with fallbacks.
/// Claude has special fallback locations; other tools just use PATH.
fn find_tool_path(tool: &str) -> Option<String> {
    crate::terminal::which_bin(tool)
}

/// Check if tool CLI is installed (PATH + fallbacks).
pub(super) fn is_tool_installed(tool: &str) -> bool {
    find_tool_path(tool).is_some()
}

pub fn will_run_in_current_terminal(
    count: usize,
    background: bool,
    run_here: Option<bool>,
    terminal: Option<&str>,
    inside_ai_tool: bool,
) -> bool {
    if let Some(rh) = run_here {
        return rh;
    }
    // terminal=here forces current terminal
    if terminal == Some("here") {
        return true;
    }
    if inside_ai_tool {
        return false;
    }
    if background {
        return false;
    }
    count == 1
}

pub fn launch_env_regime(run_here: bool, inside_ai_tool: bool) -> LaunchEnvRegime {
    if run_here {
        LaunchEnvRegime::RunHere
    } else if contaminated_parent_with_inside_ai_tool(inside_ai_tool) {
        LaunchEnvRegime::ContaminatedParent
    } else {
        LaunchEnvRegime::HumanShell
    }
}

pub fn contaminated_parent() -> bool {
    contaminated_parent_with_inside_ai_tool(crate::shared::platform::is_inside_ai_tool())
}

fn contaminated_parent_with_inside_ai_tool(inside_ai_tool: bool) -> bool {
    inside_ai_tool
        || std::env::var_os("CI").is_some()
        || std::env::var_os("GITHUB_ACTIONS").is_some()
        || std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with("CARGO_"))
}

/// Build base environment, then overlay config.toml + ~/.hcom/env (these win).
///
/// This makes the new-window/runner-script path behave like the PTY path
/// (`Command::new` inherits parent env by default). The runner script
/// already `unset`s TOOL_MARKER_VARS + HCOM_IDENTITY_VARS before exec,
/// so those categories are safe to include here (they're cleared in-script).
pub fn build_launch_env(
    hcom_config: &HcomConfig,
    regime: LaunchEnvRegime,
) -> HashMap<String, String> {
    build_launch_env_with_resolver(hcom_config, regime, crate::shell_env::resolved_shell_env)
}

pub(super) fn build_launch_env_with_resolver<F>(
    hcom_config: &HcomConfig,
    regime: LaunchEnvRegime,
    resolved_shell_env: F,
) -> HashMap<String, String>
where
    F: Fn() -> Option<HashMap<String, String>>,
{
    let base: HashMap<String, String> = match regime {
        LaunchEnvRegime::HumanShell | LaunchEnvRegime::RunHere => std::env::vars().collect(),
        LaunchEnvRegime::ContaminatedParent => {
            resolved_shell_env().unwrap_or_else(|| std::env::vars().collect())
        }
    };
    let strip = match regime {
        LaunchEnvRegime::RunHere => run_here_env_strip_set(),
        LaunchEnvRegime::HumanShell | LaunchEnvRegime::ContaminatedParent => env_strip_set(),
    };
    let mut env: HashMap<String, String> = base
        .into_iter()
        .filter(|(k, _)| !strip.contains(k.as_str()))
        .collect();

    // HCOM_* settings from config.toml
    for (key, value) in hcom_config.to_env_dict() {
        if !value.is_empty() {
            env.insert(key, value);
        }
    }

    // Passthrough vars from env file (these win over everything)
    let env_path = paths::hcom_path(&["env"]);
    for (key, value) in config::load_env_extras(&env_path) {
        if !value.is_empty() {
            env.insert(key, value);
        }
    }

    env
}

/// Build the set of env var names to strip from inherited env.
///
/// Three closed categories (owned by hcom):
/// 1. HCOM_IDENTITY_VARS
/// 2. TOOL_MARKER_VARS
/// 3. TERMINAL_CONTEXT_VARS
///
/// Per-tool instance-state vars are NOT in the initial strip — they are
/// stripped per-instance later via `strip_instance_state_vars` so that
/// cross-tool nesting doesn't strip vars the child tool doesn't own.
fn env_strip_set() -> HashSet<String> {
    let mut strip: HashSet<String> = HashSet::new();

    strip.extend(run_here_env_strip_set());
    for v in crate::terminal::TERMINAL_CONTEXT_VARS {
        strip.insert((*v).to_string());
    }

    strip
}

fn run_here_env_strip_set() -> HashSet<String> {
    let mut strip: HashSet<String> = HashSet::new();

    for v in HCOM_IDENTITY_VARS {
        strip.insert((*v).to_string());
    }
    for v in tool_marker_vars() {
        strip.insert((*v).to_string());
    }
    strip.insert("HCOM_LAUNCHED_PRESET".to_string());

    strip
}

pub(super) fn isolated_tool_config_dir(tool: &LaunchTool) -> Option<std::path::PathBuf> {
    let root = crate::runtime_env::tool_config_root();
    if dirs::home_dir().as_deref() == Some(root.as_path()) {
        return None;
    }
    let dirname = match tool.tool() {
        crate::tool::Tool::Claude => ".claude",
        crate::tool::Tool::Gemini | crate::tool::Tool::Antigravity => ".gemini",
        crate::tool::Tool::Codex => ".codex",
        crate::tool::Tool::Kilo => ".kilo",
        crate::tool::Tool::Pi => ".pi",
        crate::tool::Tool::Omp => ".omp",
        crate::tool::Tool::Cursor => ".cursor",
        crate::tool::Tool::Kimi => ".kimi",
        crate::tool::Tool::Copilot => ".copilot",
        crate::tool::Tool::OpenCode | crate::tool::Tool::Adhoc => return None,
    };
    Some(root.join(dirname))
}

/// Get system prompt file path for Gemini/Codex.
fn get_system_prompt_path(tool: &str) -> std::path::PathBuf {
    let prompts_dir = paths::hcom_path(&["system-prompts"]);
    fs::create_dir_all(&prompts_dir).ok();
    prompts_dir.join(format!("{}.md", tool))
}

/// Write system prompt to file (only if content differs).
pub(super) fn write_system_prompt_file(system_prompt: &str, tool: &str) -> String {
    let filepath = get_system_prompt_path(tool);

    // Only write if content differs
    if let Ok(existing) = fs::read_to_string(&filepath)
        && existing == system_prompt
    {
        return filepath.to_string_lossy().to_string();
    }

    if let Err(e) = fs::write(&filepath, system_prompt) {
        eprintln!(
            "[hcom] warn: failed to write system prompt to {}: {e}",
            filepath.display()
        );
    }
    filepath.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::collections::BTreeMap;

    pub(super) struct EnvVarGuard {
        saved: BTreeMap<String, Option<String>>,
    }

    impl EnvVarGuard {
        pub(super) fn clean_detection_env() -> Self {
            let mut keys: Vec<String> = [
                "CLAUDECODE",
                "CLAUDE_ENV_FILE",
                "ANTIGRAVITY_AGENT",
                "GEMINI_CLI",
                "CODEX_SANDBOX",
                "CODEX_SANDBOX_NETWORK_DISABLED",
                "CODEX_MANAGED_BY_NPM",
                "CODEX_MANAGED_BY_BUN",
                "CODEX_THREAD_ID",
                "OPENCODE",
                "KILO",
                "CURSOR_AGENT",
                "CURSOR_PROJECT_DIR",
                "KIMI_CODE_CLI",
                "KIMI_SESSION_ID",
                "HCOM_TOOL",
                "HCOM_LAUNCHED",
                "HCOM_PI",
                "CI",
                "GITHUB_ACTIONS",
                "CARGO_TEST_PARENT",
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            keys.extend(
                std::env::vars()
                    .map(|(key, _)| key)
                    .filter(|key| key.starts_with("CARGO_")),
            );
            Self::remove(keys)
        }

        pub(super) fn remove<I>(keys: I) -> Self
        where
            I: IntoIterator<Item = String>,
        {
            let mut saved = BTreeMap::new();
            for key in keys {
                saved
                    .entry(key.clone())
                    .or_insert_with(|| std::env::var(&key).ok());
                unsafe { std::env::remove_var(key) };
            }
            Self { saved }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    fn test_will_run_in_current_terminal() {
        // Explicit override
        assert!(will_run_in_current_terminal(
            5,
            false,
            Some(true),
            None,
            false
        ));
        assert!(!will_run_in_current_terminal(
            1,
            false,
            Some(false),
            None,
            false
        ));

        // terminal=here
        assert!(will_run_in_current_terminal(
            5,
            false,
            None,
            Some("here"),
            false
        ));

        // Inside AI tool → always new window
        assert!(!will_run_in_current_terminal(1, false, None, None, true));

        // Background → never run here
        assert!(!will_run_in_current_terminal(1, true, None, None, false));

        // Single → run here, multiple → new window
        assert!(will_run_in_current_terminal(1, false, None, None, false));
        assert!(!will_run_in_current_terminal(2, false, None, None, false));
    }

    #[test]
    fn test_env_strip_set_strips_closed_categories() {
        let strip = env_strip_set();
        // HCOM identity
        assert!(strip.contains("HCOM_PROCESS_ID"));
        assert!(strip.contains("HCOM_LAUNCHED"));
        // Tool markers
        assert!(strip.contains("CLAUDECODE"));
        assert!(strip.contains("CLAUDE_ENV_FILE"));
        assert!(strip.contains("CODEX_SANDBOX"));
        assert!(strip.contains("CODEX_THREAD_ID"));
        assert!(strip.contains("GEMINI_SYSTEM_MD"));
        assert!(strip.contains("HCOM_TOOL"));
        assert!(strip.contains("HCOM_PI"));
        assert!(!strip.contains("PI_CODING_AGENT_DIR"));
        // Terminal context
        assert!(strip.contains("KITTY_WINDOW_ID"));
        assert!(strip.contains("TMUX_PANE"));
        assert!(!strip.contains("COLORTERM"));
        assert!(!strip.contains("TERM"));
    }

    #[test]
    #[serial]
    fn test_contaminated_parent_detection() {
        let _guard = EnvVarGuard::clean_detection_env();
        assert!(!contaminated_parent());

        unsafe { std::env::set_var("CLAUDECODE", "1") };
        assert!(contaminated_parent());
        unsafe { std::env::remove_var("CLAUDECODE") };

        unsafe { std::env::set_var("CODEX_THREAD_ID", "thread") };
        assert!(contaminated_parent());
        unsafe { std::env::remove_var("CODEX_THREAD_ID") };

        unsafe { std::env::set_var("CI", "1") };
        assert!(contaminated_parent());
        unsafe { std::env::remove_var("CI") };

        unsafe { std::env::set_var("CARGO_TEST_PARENT", "1") };
        assert!(contaminated_parent());
    }

    #[test]
    #[serial]
    fn test_build_launch_env_inherits_parent_env() {
        unsafe { std::env::set_var("RORI_TEST_MY_VAR", "hello") }
        unsafe { std::env::set_var("RORI_TEST_OPENROUTER_API_KEY", "sk-test-123") }
        unsafe { std::env::set_var("RORI_TEST_PI_OFFLINE", "1") }

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert_eq!(
            env.get("RORI_TEST_MY_VAR").map(String::as_str),
            Some("hello")
        );
        assert_eq!(
            env.get("RORI_TEST_OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-test-123")
        );
        assert_eq!(
            env.get("RORI_TEST_PI_OFFLINE").map(String::as_str),
            Some("1")
        );

        unsafe { std::env::remove_var("RORI_TEST_MY_VAR") }
        unsafe { std::env::remove_var("RORI_TEST_OPENROUTER_API_KEY") }
        unsafe { std::env::remove_var("RORI_TEST_PI_OFFLINE") }
    }

    #[test]
    #[serial]
    fn test_build_launch_env_agent_regime_uses_resolved_shell_base() {
        let _guard = EnvVarGuard::remove(vec![
            "RORI_PARENT_CONTAMINATION".to_string(),
            "RORI_RESOLVED_AUTH".to_string(),
        ]);
        unsafe { std::env::set_var("RORI_PARENT_CONTAMINATION", "leak") };

        let config = crate::config::HcomConfig::default();
        let env =
            build_launch_env_with_resolver(&config, LaunchEnvRegime::ContaminatedParent, || {
                Some(HashMap::from([(
                    "RORI_RESOLVED_AUTH".to_string(),
                    "auth-token".to_string(),
                )]))
            });

        assert!(!env.contains_key("RORI_PARENT_CONTAMINATION"));
        assert_eq!(
            env.get("RORI_RESOLVED_AUTH").map(String::as_str),
            Some("auth-token")
        );
    }

    #[test]
    #[serial]
    fn test_build_launch_env_strips_closed_categories() {
        unsafe { std::env::set_var("HCOM_PROCESS_ID", "pid-stale") }
        unsafe { std::env::set_var("CLAUDECODE", "1") }
        unsafe { std::env::set_var("CODEX_THREAD_ID", "thread-stale") }
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", "/tmp/pi-config") }
        unsafe { std::env::set_var("KITTY_WINDOW_ID", "1337") }

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert!(!env.contains_key("HCOM_PROCESS_ID"));
        assert!(!env.contains_key("CLAUDECODE"));
        assert!(!env.contains_key("CODEX_THREAD_ID"));
        assert_eq!(
            env.get("PI_CODING_AGENT_DIR").map(String::as_str),
            Some("/tmp/pi-config")
        );
        assert!(!env.contains_key("KITTY_WINDOW_ID"));

        unsafe { std::env::remove_var("HCOM_PROCESS_ID") }
        unsafe { std::env::remove_var("CLAUDECODE") }
        unsafe { std::env::remove_var("CODEX_THREAD_ID") }
        unsafe { std::env::remove_var("PI_CODING_AGENT_DIR") }
        unsafe { std::env::remove_var("KITTY_WINDOW_ID") }
    }

    #[test]
    #[serial]
    fn test_build_launch_env_run_here_inherits_terminal_vars() {
        let _guard =
            EnvVarGuard::remove(vec!["NO_COLOR".to_string(), "HCOM_PROCESS_ID".to_string()]);
        unsafe { std::env::set_var("NO_COLOR", "1") };
        unsafe { std::env::set_var("HCOM_PROCESS_ID", "pid-stale") };

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env_with_resolver(&config, LaunchEnvRegime::RunHere, || {
            panic!("run_here must not resolve shell env")
        });

        assert_eq!(env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert!(!env.contains_key("HCOM_PROCESS_ID"));
    }

    #[test]
    #[serial]
    fn test_build_launch_env_fail_open_to_parent_env() {
        let _guard = EnvVarGuard::remove(vec!["RORI_FAIL_OPEN_PARENT".to_string()]);
        unsafe { std::env::set_var("RORI_FAIL_OPEN_PARENT", "present") };

        let config = crate::config::HcomConfig::default();
        let env =
            build_launch_env_with_resolver(&config, LaunchEnvRegime::ContaminatedParent, || None);

        assert_eq!(
            env.get("RORI_FAIL_OPEN_PARENT").map(String::as_str),
            Some("present")
        );
    }

    #[test]
    #[serial]
    fn test_build_launch_env_config_overrides_ambient() {
        unsafe { std::env::set_var("HCOM_TAG", "ambient-tag") }

        let config = crate::config::HcomConfig {
            tag: "config-tag".to_string(),
            ..Default::default()
        };
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert_eq!(env.get("HCOM_TAG").map(String::as_str), Some("config-tag"));

        unsafe { std::env::remove_var("HCOM_TAG") }
    }

    #[test]
    #[serial]
    fn test_background_runner_env_uses_upstream_resolved_base() {
        use crate::launcher::pipeline::background_runner_env;

        let _guard = EnvVarGuard::remove(vec!["RORI_BACKGROUND_CONTAMINATION".to_string()]);
        unsafe { std::env::set_var("RORI_BACKGROUND_CONTAMINATION", "leak") };

        let config = crate::config::HcomConfig::default();
        let env =
            build_launch_env_with_resolver(&config, LaunchEnvRegime::ContaminatedParent, || {
                Some(HashMap::from([(
                    "RORI_BACKGROUND_AUTH".to_string(),
                    "auth-token".to_string(),
                )]))
            });
        let runner_env = background_runner_env("codex", &env, "nita");

        assert!(!runner_env.contains_key("RORI_BACKGROUND_CONTAMINATION"));
        assert_eq!(
            runner_env.get("RORI_BACKGROUND_AUTH").map(String::as_str),
            Some("auth-token")
        );
    }

    #[test]
    #[serial]
    fn test_same_tool_nesting_strips_instance_state() {
        unsafe { std::env::set_var("GEMINI_PTY_INFO", "child_process") }
        unsafe { std::env::set_var("GEMINI_API_KEY", "parent-key") }

        let config = crate::config::HcomConfig::default();
        let mut env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        let gemini_spec: &'static crate::integration_spec::IntegrationSpec =
            crate::tool::Tool::Gemini.spec();
        for var in gemini_spec.instance_state_env {
            env.remove(*var);
        }

        assert!(!env.contains_key("GEMINI_PTY_INFO"));
        assert_eq!(
            env.get("GEMINI_API_KEY").map(String::as_str),
            Some("parent-key")
        );

        unsafe { std::env::remove_var("GEMINI_PTY_INFO") }
        unsafe { std::env::remove_var("GEMINI_API_KEY") }
    }

    #[test]
    #[serial]
    fn test_cross_tool_nesting_forwards_auth() {
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-parent") }

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert_eq!(
            env.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-parent")
        );

        unsafe { std::env::remove_var("OPENROUTER_API_KEY") }
    }
}
