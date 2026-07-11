//! Unified launcher pipeline for released AI-tool integrations.
//!
//!
//! Provides a single entry point for launching all supported AI tools
//! with consistent batch tracking, environment setup, and error handling.

use std::collections::HashMap;

use anyhow::{Result, bail};

pub mod batch;
pub mod environment;
pub mod pipeline;
pub mod preprocess;

/// Canonical tool types for launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTool {
    Claude,
    ClaudePty,
    Gemini,
    Codex,
    OpenCode,
    Kilo,
    Pi,
    Antigravity,
    Cursor,
    Kimi,
    Copilot,
    Omp,
}

impl LaunchTool {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(LaunchTool::Claude),
            "claude-pty" => Ok(LaunchTool::ClaudePty),
            "gemini" => Ok(LaunchTool::Gemini),
            "codex" => Ok(LaunchTool::Codex),
            "opencode" => Ok(LaunchTool::OpenCode),
            "kilo" | "kilocode" => Ok(LaunchTool::Kilo),
            "pi" | "pi-agent" => Ok(LaunchTool::Pi),
            "omp" | "omp-agent" => Ok(LaunchTool::Omp),
            "antigravity" | "agy" => Ok(LaunchTool::Antigravity),
            "cursor" | "cursor-agent" => Ok(LaunchTool::Cursor),
            "kimi" => Ok(LaunchTool::Kimi),
            "copilot" => Ok(LaunchTool::Copilot),
            _ => bail!("Unknown tool: {}", s),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LaunchTool::Claude => "claude",
            LaunchTool::ClaudePty => "claude-pty",
            LaunchTool::Gemini => "gemini",
            LaunchTool::Codex => "codex",
            LaunchTool::OpenCode => "opencode",
            LaunchTool::Kilo => "kilo",
            LaunchTool::Pi => "pi",
            LaunchTool::Omp => "omp",
            LaunchTool::Antigravity => "antigravity",
            LaunchTool::Cursor => "cursor",
            LaunchTool::Kimi => "kimi",
            LaunchTool::Copilot => "copilot",
        }
    }

    /// Canonical [`Tool`] for this launch surface.
    ///
    /// `ClaudePty` is a launch-surface alias (PTY-wrapped Claude); it resolves
    /// to `Tool::Claude` so all per-tool data flows through one spec.
    pub fn tool(&self) -> crate::tool::Tool {
        match self {
            LaunchTool::Claude | LaunchTool::ClaudePty => crate::tool::Tool::Claude,
            LaunchTool::Gemini => crate::tool::Tool::Gemini,
            LaunchTool::Codex => crate::tool::Tool::Codex,
            LaunchTool::OpenCode => crate::tool::Tool::OpenCode,
            LaunchTool::Kilo => crate::tool::Tool::Kilo,
            LaunchTool::Pi => crate::tool::Tool::Pi,
            LaunchTool::Omp => crate::tool::Tool::Omp,
            LaunchTool::Antigravity => crate::tool::Tool::Antigravity,
            LaunchTool::Cursor => crate::tool::Tool::Cursor,
            LaunchTool::Kimi => crate::tool::Tool::Kimi,
            LaunchTool::Copilot => crate::tool::Tool::Copilot,
        }
    }

    /// Integration spec for this launch surface (shared with the base `Tool`).
    pub fn spec(&self) -> &'static crate::integration_spec::IntegrationSpec {
        self.tool().spec()
    }

    /// Base tool name (without -pty suffix). Equivalent to `self.tool().as_str()`.
    pub fn base_tool(&self) -> &'static str {
        self.tool().as_str()
    }

    /// Whether this tool uses the PTY wrapper.
    pub fn uses_pty(&self) -> bool {
        // ClaudePty is a launch surface (alias), not a Tool variant — it always
        // takes the PTY path. Everything else defers to the canonical spec.
        if matches!(self, LaunchTool::ClaudePty) {
            return true;
        }
        self.spec().launch.uses_pty_default
    }

    /// Executable name on PATH for this tool.
    pub fn cli_binary(&self) -> &'static str {
        self.spec().cli_binary
    }
}

/// How the child process is hosted. Computed from (tool, background, pty) at
/// launch time so dispatch doesn't have to re-derive the combination.
///
/// - `InteractiveVisible`: foreground, user-visible terminal. All tools.
/// - `HeadlessPty`:       background, PTY wrapper in a detached runner. Default
///   for gemini/codex/opencode/kilo/pi/omp/antigravity/cursor/kimi/copilot and for default claude `--headless`.
/// - `NativePrint`:       background, direct claude spawn in print mode
///   (`-p --output-format stream-json --verbose`). Claude only, opt-in via an
///   explicit `-p`/`--print`; kept alive across turns by hcom's stop-hook loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchBackend {
    InteractiveVisible,
    HeadlessPty,
    NativePrint,
}

impl LaunchBackend {
    /// Resolve from the already-prepared (tool, background) pair.
    ///
    /// The PTY-vs-print decision for Claude is encoded in the [`LaunchTool`]
    /// surface chosen by `launch()`: `Claude` is the `-p`/`--print` surface
    /// (→ `NativePrint`), `ClaudePty` is the live PTY surface (→ `HeadlessPty`).
    /// Background Claude defaults to `ClaudePty`; it only becomes
    /// `Claude`/`NativePrint` when the caller passes `-p`/`--print`.
    pub fn resolve(tool: &LaunchTool, background: bool) -> Self {
        if !background {
            return LaunchBackend::InteractiveVisible;
        }
        match tool {
            LaunchTool::Claude => LaunchBackend::NativePrint,
            LaunchTool::ClaudePty => LaunchBackend::HeadlessPty,
            LaunchTool::Gemini
            | LaunchTool::Codex
            | LaunchTool::OpenCode
            | LaunchTool::Kilo
            | LaunchTool::Pi
            | LaunchTool::Omp
            | LaunchTool::Antigravity
            | LaunchTool::Cursor
            | LaunchTool::Kimi
            | LaunchTool::Copilot => LaunchBackend::HeadlessPty,
        }
    }
}

/// Launch parameters.
#[derive(Clone)]
pub struct LaunchParams {
    pub tool: String,
    pub count: usize,
    pub args: Vec<String>,
    /// Raw user/config args to persist for future resume, before hcom injections.
    pub persisted_args: Option<Vec<String>>,
    /// Session id being resumed, inherited by the recreated instance row so a
    /// kill before the tool's first turn (no hook re-bind yet) stays resumable.
    pub prior_session_id: Option<String>,
    pub tag: Option<String>,
    pub system_prompt: Option<String>,
    pub initial_prompt: Option<String>,
    pub background: bool,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub launcher: Option<String>,
    pub run_here: Option<bool>,
    pub batch_id: Option<String>,
    pub name: Option<String>,
    pub skip_validation: bool,
    pub terminal: Option<String>,
    pub append_reply_handoff: bool,
}

impl Default for LaunchParams {
    fn default() -> Self {
        Self {
            tool: "claude".to_string(),
            count: 1,
            args: Vec::new(),
            persisted_args: None,
            prior_session_id: None,
            tag: None,
            system_prompt: None,
            initial_prompt: None,
            background: false,
            cwd: None,
            env: None,
            launcher: None,
            run_here: None,
            batch_id: None,
            name: None,
            skip_validation: false,
            terminal: None,
            append_reply_handoff: true,
        }
    }
}

/// Launch result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LaunchResult {
    pub tool: String,
    pub batch_id: String,
    pub launched: usize,
    pub failed: usize,
    pub background: bool,
    pub log_files: Vec<String>,
    pub handles: Vec<serde_json::Value>,
    pub errors: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchEnvRegime {
    HumanShell,
    ContaminatedParent,
    RunHere,
}

// Re-exports — preserve the original `crate::launcher::X` API.
pub use environment::build_launch_env;
pub use environment::contaminated_parent;
pub use environment::launch_env_regime;
pub use environment::will_run_in_current_terminal;
pub use pipeline::create_runner_script;
pub use pipeline::launch;
pub(crate) use preprocess::validate_tool_args;
