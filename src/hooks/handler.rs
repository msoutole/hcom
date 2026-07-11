//! Hook handler trait — unified dispatch for argv-based and stdin-based tool hooks.
//!
//! Supports two input modes:
//! - `Argv`: tool receives hook name + command-line args (Pi, OMP, OpenCode)
//! - `StdinJson`: tool reads JSON from stdin, parsed into a `HookPayload` (Claude, Gemini, etc.)
//!
//! The orchestrator (`run_hook_handler`) handles context building, DB open, gate check,
//! input reading, payload parsing, panic guard, timing log, and output — eliminating
//! ~40 lines of boilerplate per tool.

use std::io::Read as _;
use std::time::Instant;

use serde_json::Value;

use crate::db::HcomDb;
use crate::hooks::HookPayload;
use crate::hooks::common;
use crate::log::{log_error, log_info, log_warn};
use crate::shared::context::HcomContext;

/// How the hook handler receives its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInputKind {
    /// Read stdin as JSON, parse into HookPayload via `parse_payload`.
    StdinJson,
    /// Receive hook parameters via argv.
    Argv,
}

/// Trait that each tool's hook dispatch implements.
pub trait HookHandler {
    /// Tool name string (e.g. "pi", "claude", "copilot").
    fn tool_name(&self) -> &'static str;

    /// How this tool receives hook input.
    fn input_kind(&self) -> HookInputKind;

    /// Core dispatch logic. Called after context/DB/gate are set up.
    ///
    /// - For `StdinJson` tools: `payload` is populated, `argv` is empty.
    /// - For `Argv` tools: `payload` is default/empty, `argv` has the stripped args.
    ///
    /// Returns (exit_code, stdout_output_string).
    fn dispatch(
        &self,
        hook_name: &str,
        ctx: &HcomContext,
        db: &HcomDb,
        payload: &HookPayload,
        argv: &[String],
    ) -> (i32, String);

    // ── Optional overrides ──

    /// Build HookPayload from raw stdin JSON.
    ///
    /// Default implementation returns a generic payload with empty fields.
    /// Override to use tool-specific factory methods like `HookPayload::from_copilot_native()`.
    fn parse_payload(&self, hook_name: &str, raw: &Value) -> HookPayload {
        let _ = hook_name;
        HookPayload {
            raw: raw.clone(),
            ..Default::default()
        }
    }

    /// Pre-dispatch gate check. Return `false` to skip dispatch entirely (exit 0, no output).
    ///
    /// Default returns `true` (always proceed).
    /// Override for tools that need to skip non-participant sessions (e.g. Gemini/Kimi).
    fn pre_dispatch_check(&self, _hook_name: &str, _ctx: &HcomContext, _db: &HcomDb) -> bool {
        true
    }

    /// Strip the hook name from argv.
    ///
    /// Default: removes argv[0] (the hook name itself).
    fn strip_argv(&self, hook_name: &str, argv: &[String]) -> Vec<String> {
        if !argv.is_empty() && argv[0] == hook_name {
            argv[1..].to_vec()
        } else {
            argv.to_vec()
        }
    }
}

/// Run any tool's hook dispatch with consistent infrastructure.
///
/// Handles: input reading, context building, DB open, gate check,
/// payload parsing, argv stripping, panic guard, timing log, and output.
///
/// Returns (exit_code, stdout_output_string).
/// - For `Argv` tools: stdout_output_string is the handler's JSON response.
/// - For `StdinJson` tools: stdout_output_string is the serialized JSON to write.
///   The caller (or router) writes this to stdout.
pub fn run_hook_handler(
    handler: &dyn HookHandler,
    hook_name: &str,
    argv: &[String],
) -> (i32, String) {
    let start = Instant::now();

    // 1. Build context + ensure directories
    let ctx = HcomContext::from_os();
    crate::paths::ensure_hcom_directories_at(&ctx.hcom_dir);

    // 2. Open DB
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log_error(
                "hooks",
                &format!("{}.db_open_error", handler.tool_name()),
                &e.to_string(),
            );
            return (0, String::new());
        }
    };

    // 3. Gate check: non-participants with empty DB → exit 0, no output
    if !common::hook_gate_check(&ctx, &db) {
        return (0, String::new());
    }

    // 4. Read input + parse payload (based on input kind)
    let (payload, stripped_argv) = match handler.input_kind() {
        HookInputKind::StdinJson => {
            let mut buf = Vec::new();
            if std::io::stdin().read_to_end(&mut buf).is_err() {
                return (0, String::new());
            }
            let raw: Value = match serde_json::from_slice(&buf) {
                Ok(v) => v,
                Err(_) => return (0, String::new()),
            };
            let payload = handler.parse_payload(hook_name, &raw);
            (payload, Vec::new())
        }
        HookInputKind::Argv => {
            let stripped = handler.strip_argv(hook_name, argv);
            (HookPayload::default(), stripped)
        }
    };

    // 5. Pre-dispatch check
    if !handler.pre_dispatch_check(hook_name, &ctx, &db) {
        return (0, String::new());
    }

    // 6. Dispatch with panic guard
    let hook_name_owned = hook_name.to_string();
    let handler_start = Instant::now();
    let (exit_code, output) = common::dispatch_with_panic_guard(
        handler.tool_name(),
        &hook_name_owned,
        (0, String::new()),
        || handler.dispatch(&hook_name_owned, &ctx, &db, &payload, &stripped_argv),
    );

    // 7. Timing log
    log_info(
        "hooks",
        &format!("{}.dispatch.timing", handler.tool_name()),
        &format!(
            "hook={} handler_ms={:.2} total_ms={:.2} exit_code={}",
            hook_name,
            handler_start.elapsed().as_secs_f64() * 1000.0,
            start.elapsed().as_secs_f64() * 1000.0,
            exit_code
        ),
    );

    (exit_code, output)
}

/// Result of preparing a stdin-based hook dispatch.
pub struct StdinDispatch {
    pub start: Instant,
    pub ctx: HcomContext,
    pub db: HcomDb,
    pub raw: Value,
}

/// Common preamble for stdin-based hook dispatch:
/// 1. Read stdin → parse as JSON
/// 2. Build HcomContext
/// 3. Open HcomDb
/// 4. Run gate check
///
/// Returns `None` (caller should return 0) on any failure.
/// Logs errors with the given tool name prefix.
pub fn prepare_stdin(tool: &str, hook_name: &str) -> Option<StdinDispatch> {
    let start = Instant::now();
    let ctx = HcomContext::from_os();

    let mut input = Vec::new();
    let raw: Value = match std::io::stdin().read_to_end(&mut input) {
        Ok(_) => match serde_json::from_slice(&input) {
            Ok(v) => v,
            Err(e) => {
                log_error(
                    "hooks",
                    &format!("{}.parse_error", tool),
                    &format!("hook={} err={}", hook_name, e),
                );
                return None;
            }
        },
        Err(e) => {
            log_error(
                "hooks",
                &format!("{}.stdin_error", tool),
                &format!("hook={} err={}", hook_name, e),
            );
            return None;
        }
    };

    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log_warn(
                "hooks",
                &format!("{}.db_error", tool),
                &format!("hook={} err={}", hook_name, e),
            );
            return None;
        }
    };

    if !common::hook_gate_check(&ctx, &db) {
        return None;
    }

    Some(StdinDispatch {
        start,
        ctx,
        db,
        raw,
    })
}
