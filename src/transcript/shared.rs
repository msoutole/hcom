//! Shared types and helpers used by all transcript parsers.

use std::path::Path;

use regex::Regex;
use serde_json::{Value, json};

/// An exchange in a transcript.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub position: usize,
    pub user: String,
    pub action: String,
    pub files: Vec<String>,
    pub timestamp: String,
    pub tools: Vec<ToolUse>,
    pub edits: Vec<Value>,
    pub errors: Vec<Value>,
    pub ended_on_error: bool,
}

/// A tool use within an exchange.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub name: String,
    pub is_error: bool,
    pub file: Option<String>,
    pub command: Option<String>,
}

/// Lazy-initialized error detection regex.
/// Uses `(?:^|\W)error:` to match "error:" not preceded by a word character
/// (lookbehinds not supported by the regex crate).
pub(crate) fn error_patterns() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(rejected|interrupted|traceback|failed|exception)\b|(?:^|\W)error:|command failed with exit code|Traceback \(most recent call last\)").unwrap()
    })
}

/// Check if a tool result indicates an error.
pub(crate) fn is_error_result(result: &Value) -> bool {
    if result
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let content = result.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if content.is_empty() {
        return false;
    }
    let check = truncate_str(content, 500);
    error_patterns().is_match(check)
}

/// Check if Codex tool output indicates an error.
pub(crate) fn codex_is_error(output: &str) -> bool {
    if output.is_empty() {
        return false;
    }
    if output.starts_with("Exit code:") {
        let exit_line = output.lines().next().unwrap_or("");
        if !exit_line.contains("Exit code: 0") {
            return true;
        }
    }
    let check = truncate_str(output, 200);
    error_patterns().is_match(check)
}

/// Extract text from tool_result content that may be a string or array of text blocks.
pub(crate) fn extract_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for block in arr {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Extract edit info from toolUseResult and/or tool_use input.
pub(crate) fn extract_edit_info(
    tool_use_result: &Option<Value>,
    tool_input: &Value,
) -> Option<Value> {
    // Try toolUseResult first
    if let Some(result) = tool_use_result.as_ref().and_then(|v| v.as_object())
        && (result.contains_key("structuredPatch") || result.contains_key("oldString"))
    {
        let file = result
            .get("filePath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let diff = if let Some(patch) = result.get("structuredPatch").and_then(|v| v.as_array()) {
            format_structured_patch(patch)
        } else if let (Some(old), Some(new)) = (
            result.get("oldString").and_then(|v| v.as_str()),
            result.get("newString").and_then(|v| v.as_str()),
        ) {
            let old_preview = truncate_str(old, 100);
            let new_preview = truncate_str(new, 100);
            let old_suffix = if old.len() > 100 { "..." } else { "" };
            let new_suffix = if new.len() > 100 { "..." } else { "" };
            format!("-{old_preview}{old_suffix}\n+{new_preview}{new_suffix}")
        } else {
            String::new()
        };
        return Some(json!({"file": file, "diff": diff}));
    }

    // Fallback: extract from tool_use input
    if let Some(obj) = tool_input.as_object()
        && (obj.contains_key("old_string") || obj.contains_key("new_string"))
    {
        let file = obj.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        let old = obj.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
        let new = obj.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        let old_preview = truncate_str(old, 100);
        let new_preview = truncate_str(new, 100);
        let old_suffix = if old.len() > 100 { "..." } else { "" };
        let new_suffix = if new.len() > 100 { "..." } else { "" };
        return Some(
            json!({"file": file, "diff": format!("-{old_preview}{old_suffix}\n+{new_preview}{new_suffix}")}),
        );
    }

    None
}

/// Format structuredPatch into readable diff.
pub(crate) fn format_structured_patch(patch: &[Value]) -> String {
    let mut lines = Vec::new();
    for hunk in patch {
        if let Some(obj) = hunk.as_object() {
            let old_start = obj.get("oldStart").and_then(|v| v.as_u64()).unwrap_or(0);
            let new_start = obj.get("newStart").and_then(|v| v.as_u64()).unwrap_or(0);
            lines.push(format!("@@ -{old_start} +{new_start} @@"));
            if let Some(hunk_lines) = obj.get("lines").and_then(|v| v.as_array()) {
                for (i, line) in hunk_lines.iter().enumerate() {
                    if i >= 20 {
                        lines.push(format!("  ... +{} more lines", hunk_lines.len() - 20));
                        break;
                    }
                    if let Some(s) = line.as_str() {
                        lines.push(s.to_string());
                    }
                }
            }
        }
    }
    lines.join("\n")
}

/// Truncate a string to at most `max` bytes at a valid UTF-8 char boundary.
pub(crate) fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Normalize tool names across agents to canonical Claude names.
pub(crate) fn normalize_tool_name(name: &str) -> &str {
    match name {
        "run_shell_command" | "shell" | "shell_command" | "bash" => "Bash",
        "read_file" | "read" | "read_many_files" => "Read",
        "write_file" | "write" => "Write",
        "edit_file" | "edit" | "apply_patch" | "replace" => "Edit",
        "search_files" | "grep" | "grep_search" => "Grep",
        "list_files" | "list_directory" | "glob" => "Glob",
        "fetch" => "WebFetch",
        "skill" => "Skill",
        _ => name,
    }
}

pub(crate) fn summarize_tool_names(tools: &[ToolUse]) -> String {
    let mut names = Vec::new();
    for tool in tools {
        if !names.contains(&tool.name) {
            names.push(tool.name.clone());
        }
    }

    match names.len() {
        0 => "tools".to_string(),
        1..=3 => names.join(", "),
        _ => format!("{}, +{} more", names[..3].join(", "), names.len() - 3),
    }
}

pub(crate) fn finalize_action_text(
    action: &str,
    tools: &[ToolUse],
    errors: &[Value],
    ended_on_error: bool,
) -> String {
    let trimmed = action.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }

    let _ = errors;
    if ended_on_error {
        if tools.is_empty() {
            "(turn ended in error)".to_string()
        } else {
            format!(
                "(turn ended in error after using {})",
                summarize_tool_names(tools)
            )
        }
    } else if !tools.is_empty() {
        format!("(tool-only turn: {})", summarize_tool_names(tools))
    } else {
        "(no response)".to_string()
    }
}

pub(crate) fn is_codex_system_injected_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions")
        || trimmed.starts_with("# AGENTS.md")
}

pub(crate) fn extract_codex_event_message_text(payload: &Value) -> String {
    if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
        return text.trim().to_string();
    }
    extract_text_content(payload)
}

pub(crate) fn same_trimmed_text(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

pub(crate) fn is_no_response_action(action: &str) -> bool {
    action.trim() == "(no response)"
}

pub(crate) fn merge_exchange_metadata(dst: &mut Exchange, src: Exchange) {
    for file in src.files {
        if !dst.files.contains(&file) {
            dst.files.push(file);
        }
    }
    dst.files = dedup_sorted_capped(&dst.files, 5);

    for tool in src.tools {
        dst.tools.push(tool);
    }
    for edit in src.edits {
        dst.edits.push(edit);
    }
    for error in src.errors {
        dst.errors.push(error);
    }
    dst.ended_on_error = src.ended_on_error;
}

pub(crate) fn collapse_codex_duplicate_exchanges(exchanges: Vec<Exchange>) -> Vec<Exchange> {
    let mut collapsed: Vec<Exchange> = Vec::new();

    for ex in exchanges {
        if let Some(last) = collapsed.last_mut() {
            let same_user = same_trimmed_text(&last.user, &ex.user);
            let same_action = same_trimmed_text(&last.action, &ex.action);

            if same_user
                && is_no_response_action(&last.action)
                && !is_no_response_action(&ex.action)
            {
                last.action = ex.action.clone();
                last.timestamp = ex.timestamp.clone();
                merge_exchange_metadata(last, ex);
                continue;
            }

            if same_user && same_action && !is_no_response_action(&last.action) {
                merge_exchange_metadata(last, ex);
                continue;
            }
        }

        collapsed.push(ex);
    }

    for (idx, ex) in collapsed.iter_mut().enumerate() {
        ex.position = idx + 1;
    }

    collapsed
}

/// Deduplicate, sort, and cap a list of file names.
pub(crate) fn dedup_sorted_capped(files: &[String], cap: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for f in files {
        if !seen.contains(f) {
            seen.push(f.clone());
        }
    }
    seen.sort();
    seen.truncate(cap);
    seen
}

/// Check if a message has actual user text (not just tool_result blocks).
pub(crate) fn has_user_text(msg: &Value) -> bool {
    let content = msg.get("content");
    if let Some(text) = content.and_then(|v| v.as_str()) {
        return !text.trim().is_empty();
    }
    if let Some(arr) = content.and_then(|v| v.as_array()) {
        return arr.iter().any(|block| {
            block.get("type").and_then(|v| v.as_str()) == Some("text")
                && block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
        });
    }
    false
}

/// Extract user text from a Gemini user message.
/// Prefers displayContent (user-visible text without hook context),
/// falls back to content (string or array of {text: ...} blocks).
pub(crate) fn extract_gemini_user_text(msg: &Value) -> String {
    // displayContent: the user-visible text (excludes hook_context injections)
    if let Some(arr) = msg.get("displayContent").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    // Fallback: content as string
    if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    // Fallback: content as array of text blocks
    if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        return parts.join("\n");
    }
    String::new()
}

pub(crate) fn extract_text_content(msg: &Value) -> String {
    if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
        return text.trim().to_string();
    }
    if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            // Skip tool_result blocks (they're not user text)
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                continue;
            }
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        return parts.join("\n");
    }
    // Fallback: look for text directly
    msg.get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Read file to string with lossy UTF-8 conversion (handles binary/corrupted files).
pub(crate) fn read_file_lossy(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read transcript: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ── Formatting (Exchange → String) ───────────────────────────────────────

/// Format exchanges for display.
pub fn format_exchanges(
    exchanges: &[Exchange],
    _instance: &str,
    full: bool,
    detailed: bool,
) -> String {
    let mut lines = Vec::new();

    for ex in exchanges {
        let user_text = if full || ex.user.len() <= 300 {
            ex.user.clone()
        } else {
            format!("{}...", truncate_str(&ex.user, 297))
        };

        let action_text = if full {
            ex.action.clone()
        } else {
            summarize_action(&ex.action)
        };

        lines.push(format!("[{}] USER: {}", ex.position, user_text));
        lines.push(format!("ASSISTANT: {}", action_text));

        if !ex.files.is_empty() {
            lines.push(format!("FILES: {}", ex.files.join(", ")));
        }

        if detailed && !ex.tools.is_empty() {
            for tool in &ex.tools {
                let marker = if tool.is_error { "  ✗" } else { "  ├─" };
                let detail = tool
                    .file
                    .as_deref()
                    .or(tool.command.as_deref())
                    .unwrap_or("");
                lines.push(format!("{marker} {} {detail}", tool.name));
            }
        }

        lines.push(String::new()); // blank line between exchanges
    }

    // Trailing hint
    if !exchanges.is_empty() {
        if !full {
            lines.push("Note: Output truncated. Use --full for full text.".to_string());
        } else {
            lines.push(
                "Note: Tool outputs & file edits hidden. Use --detailed for full details."
                    .to_string(),
            );
        }
    }

    lines.join("\n")
}

/// Summarize action text (first 3 lines, strip prefixes).
pub fn summarize_action(text: &str) -> String {
    if text.is_empty() {
        return "(no response)".to_string();
    }
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        return "(no response)".to_string();
    }
    // Strip common prefixes
    for prefix in &["I'll ", "I will ", "Let me ", "Sure, ", "Okay, ", "OK, "] {
        if lines[0].starts_with(prefix) {
            lines[0] = lines[0][prefix.len()..].to_string();
            break;
        }
    }
    let summary = lines.join(" ");
    if summary.len() > 200 {
        format!("{}...", truncate_str(&summary, 197))
    } else if text.lines().filter(|l| !l.trim().is_empty()).count() > 3 {
        format!("{summary} ...")
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── truncate_str ──────────────────────────────────────────────────────

    #[test]
    fn truncate_str_no_truncation_needed() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_at_byte_boundary() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_str_at_multi_byte_boundary() {
        assert_eq!(truncate_str("héllo", 3), "hé");
    }

    #[test]
    fn truncate_str_before_emoji() {
        assert_eq!(truncate_str("a🔥b", 2), "a");
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn truncate_str_max_zero() {
        assert_eq!(truncate_str("abc", 0), "");
    }

    // ── normalize_tool_name ───────────────────────────────────────────────

    #[test]
    fn normalize_tool_name_bash_aliases() {
        assert_eq!(normalize_tool_name("run_shell_command"), "Bash");
        assert_eq!(normalize_tool_name("shell"), "Bash");
        assert_eq!(normalize_tool_name("bash"), "Bash");
    }

    #[test]
    fn normalize_tool_name_write_aliases() {
        assert_eq!(normalize_tool_name("write_file"), "Write");
        assert_eq!(normalize_tool_name("write"), "Write");
    }

    #[test]
    fn normalize_tool_name_edit_aliases() {
        assert_eq!(normalize_tool_name("edit_file"), "Edit");
    }

    #[test]
    fn normalize_tool_name_read_aliases() {
        assert_eq!(normalize_tool_name("read_file"), "Read");
    }

    #[test]
    fn normalize_tool_name_skill_and_fetch() {
        assert_eq!(normalize_tool_name("skill"), "Skill");
        assert_eq!(normalize_tool_name("fetch"), "WebFetch");
    }

    #[test]
    fn normalize_tool_name_unknown_passthrough() {
        assert_eq!(normalize_tool_name("unknown_tool"), "unknown_tool");
    }

    // ── dedup_sorted_capped ───────────────────────────────────────────────

    #[test]
    fn dedup_sorted_capped_sorts_and_dedups() {
        let input: Vec<String> = vec!["b".into(), "a".into(), "c".into()];
        let result = dedup_sorted_capped(&input, 5);
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn dedup_sorted_capped_caps_at_limit() {
        let input: Vec<String> = vec!["a".into(), "b".into(), "a".into(), "c".into()];
        let result = dedup_sorted_capped(&input, 2);
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn dedup_sorted_capped_all_duplicates() {
        let input: Vec<String> = vec!["a".into(), "a".into(), "a".into()];
        let result = dedup_sorted_capped(&input, 5);
        assert_eq!(result, vec!["a"]);
    }

    #[test]
    fn dedup_sorted_capped_empty() {
        let input: Vec<String> = vec![];
        let result = dedup_sorted_capped(&input, 5);
        assert!(result.is_empty());
    }

    // ── same_trimmed_text ─────────────────────────────────────────────────

    #[test]
    fn same_trimmed_text_equal() {
        assert!(same_trimmed_text("hello", "hello"));
    }

    #[test]
    fn same_trimmed_text_with_whitespace() {
        assert!(same_trimmed_text("  hello  ", "hello"));
    }

    #[test]
    fn same_trimmed_text_different() {
        assert!(!same_trimmed_text("hello", "world"));
    }

    // ── is_no_response_action ─────────────────────────────────────────────

    #[test]
    fn is_no_response_action_exact() {
        assert!(is_no_response_action("(no response)"));
    }

    #[test]
    fn is_no_response_action_with_trailing_space() {
        assert!(is_no_response_action("(no response)  "));
    }

    #[test]
    fn is_no_response_action_other_text() {
        assert!(!is_no_response_action("hello"));
    }

    #[test]
    fn is_no_response_action_empty() {
        assert!(!is_no_response_action(""));
    }

    // ── is_error_result ──────────────────────────────────────────────────

    #[test]
    fn is_error_result_explicit_true() {
        let v = json!({"is_error": true});
        assert!(is_error_result(&v));
    }

    #[test]
    fn is_error_result_explicit_false() {
        let v = json!({"is_error": false});
        assert!(!is_error_result(&v));
    }

    #[test]
    fn is_error_result_no_error_text() {
        let v = json!({"content": "some output"});
        assert!(!is_error_result(&v));
    }

    #[test]
    fn is_error_result_empty_object() {
        let v = json!({});
        assert!(!is_error_result(&v));
    }

    #[test]
    fn is_error_result_traceback() {
        let v = json!({"content": "Traceback (most recent call last)"});
        assert!(is_error_result(&v));
    }

    #[test]
    fn is_error_result_error_prefix() {
        let v = json!({"content": "Error: something failed"});
        assert!(is_error_result(&v));
    }

    // ── codex_is_error ────────────────────────────────────────────────────

    #[test]
    fn codex_is_error_empty() {
        assert!(!codex_is_error(""));
    }

    #[test]
    fn codex_is_error_exit_zero() {
        assert!(!codex_is_error("Exit code: 0"));
    }

    #[test]
    fn codex_is_error_exit_nonzero() {
        assert!(codex_is_error("Exit code: 1"));
    }

    #[test]
    fn codex_is_error_exit_zero_with_error_text() {
        assert!(!codex_is_error("Exit code: 0\nsome error text"));
    }

    #[test]
    fn codex_is_error_pattern_match() {
        assert!(codex_is_error("failed to connect"));
    }

    // ── has_user_text ────────────────────────────────────────────────────

    #[test]
    fn has_user_text_string_content() {
        let v = json!({"content": "hello"});
        assert!(has_user_text(&v));
    }

    #[test]
    fn has_user_text_empty_string() {
        let v = json!({"content": ""});
        assert!(!has_user_text(&v));
    }

    #[test]
    fn has_user_text_only_whitespace() {
        let v = json!({"content": "   "});
        assert!(!has_user_text(&v));
    }

    #[test]
    fn has_user_text_text_block() {
        let v = json!({"content": [{"type": "text", "text": "hello"}]});
        assert!(has_user_text(&v));
    }

    #[test]
    fn has_user_text_tool_result_only() {
        let v = json!({"content": [{"type": "tool_result", "text": "output"}]});
        assert!(!has_user_text(&v));
    }

    #[test]
    fn has_user_text_no_content() {
        let v = json!({});
        assert!(!has_user_text(&v));
    }

    // ── finalize_action_text ──────────────────────────────────────────────

    #[test]
    fn finalize_action_text_non_empty() {
        assert_eq!(
            finalize_action_text("hello world", &[], &[], false),
            "hello world"
        );
    }

    #[test]
    fn finalize_action_text_empty_no_tools_no_error() {
        assert_eq!(finalize_action_text("", &[], &[], false), "(no response)");
    }

    #[test]
    fn finalize_action_text_tool_only_turn() {
        let tools = vec![ToolUse {
            name: "Bash".into(),
            is_error: false,
            file: None,
            command: None,
        }];
        assert_eq!(
            finalize_action_text("", &tools, &[], false),
            "(tool-only turn: Bash)"
        );
    }

    #[test]
    fn finalize_action_text_ended_in_error() {
        let errors = vec![json!("error")];
        assert_eq!(
            finalize_action_text("", &[], &errors, true),
            "(turn ended in error)"
        );
    }

    #[test]
    fn finalize_action_text_ended_in_error_after_tools() {
        let tools = vec![ToolUse {
            name: "Bash".into(),
            is_error: false,
            file: None,
            command: None,
        }];
        let errors = vec![json!("error")];
        assert_eq!(
            finalize_action_text("", &tools, &errors, true),
            "(turn ended in error after using Bash)"
        );
    }

    // ── summarize_tool_names ──────────────────────────────────────────────

    #[test]
    fn summarize_tool_names_empty() {
        assert_eq!(summarize_tool_names(&[]), "tools");
    }

    #[test]
    fn summarize_tool_names_one_tool() {
        let tools = vec![ToolUse {
            name: "Bash".into(),
            is_error: false,
            file: None,
            command: None,
        }];
        assert_eq!(summarize_tool_names(&tools), "Bash");
    }

    #[test]
    fn summarize_tool_names_multiple_tools() {
        let tools = vec![
            ToolUse {
                name: "Bash".into(),
                is_error: false,
                file: None,
                command: None,
            },
            ToolUse {
                name: "Read".into(),
                is_error: false,
                file: None,
                command: None,
            },
            ToolUse {
                name: "Edit".into(),
                is_error: false,
                file: None,
                command: None,
            },
        ];
        assert_eq!(summarize_tool_names(&tools), "Bash, Read, Edit");
    }

    #[test]
    fn summarize_tool_names_more_than_three() {
        let tools = vec![
            ToolUse {
                name: "A".into(),
                is_error: false,
                file: None,
                command: None,
            },
            ToolUse {
                name: "B".into(),
                is_error: false,
                file: None,
                command: None,
            },
            ToolUse {
                name: "C".into(),
                is_error: false,
                file: None,
                command: None,
            },
            ToolUse {
                name: "D".into(),
                is_error: false,
                file: None,
                command: None,
            },
        ];
        assert_eq!(summarize_tool_names(&tools), "A, B, C, +1 more");
    }

    // ── extract_text_content ──────────────────────────────────────────────

    #[test]
    fn extract_text_content_string() {
        let v = json!({"content": "hello"});
        assert_eq!(extract_text_content(&v), "hello");
    }

    #[test]
    fn extract_text_content_text_block() {
        let v = json!({"content": [{"type": "text", "text": "hello"}]});
        assert_eq!(extract_text_content(&v), "hello");
    }

    #[test]
    fn extract_text_content_skips_tool_result() {
        let v = json!({"content": [{"type": "tool_result", "text": "output"}]});
        assert_eq!(extract_text_content(&v), "");
    }

    #[test]
    fn extract_text_content_fallback_to_text() {
        let v = json!({"text": "direct"});
        assert_eq!(extract_text_content(&v), "direct");
    }

    #[test]
    fn extract_text_content_empty() {
        let v = json!({});
        assert_eq!(extract_text_content(&v), "");
    }

    // ── extract_gemini_user_text ──────────────────────────────────────────

    #[test]
    fn extract_gemini_user_text_prefers_display_content() {
        let v = json!({
            "displayContent": [{"text": "hello"}],
            "content": "fallback"
        });
        assert_eq!(extract_gemini_user_text(&v), "hello");
    }

    #[test]
    fn extract_gemini_user_text_content_string() {
        let v = json!({"content": "fallback"});
        assert_eq!(extract_gemini_user_text(&v), "fallback");
    }

    #[test]
    fn extract_gemini_user_text_content_array() {
        let v = json!({"content": [{"text": "array text"}]});
        assert_eq!(extract_gemini_user_text(&v), "array text");
    }

    // ── collapse_codex_duplicate_exchanges ────────────────────────────────

    fn make_exchange(position: usize, user: &str, action: &str, files: Vec<&str>) -> Exchange {
        Exchange {
            position,
            user: user.into(),
            action: action.into(),
            files: files.into_iter().map(String::from).collect(),
            timestamp: String::new(),
            tools: vec![],
            edits: vec![],
            errors: vec![],
            ended_on_error: false,
        }
    }

    #[test]
    fn collapse_codex_duplicate_exchanges_empty() {
        let result = collapse_codex_duplicate_exchanges(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn collapse_codex_duplicate_exchanges_single() {
        let ex = make_exchange(1, "user", "hello", vec![]);
        let result = collapse_codex_duplicate_exchanges(vec![ex]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].user, "user");
        assert_eq!(result[0].action, "hello");
    }

    #[test]
    fn collapse_codex_duplicate_exchanges_merges_identical() {
        let ex1 = make_exchange(1, "user", "hello", vec!["a"]);
        let ex2 = make_exchange(2, "user", "hello", vec!["b"]);
        let result = collapse_codex_duplicate_exchanges(vec![ex1, ex2]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].position, 1);
        assert!(result[0].files.contains(&"a".to_string()));
        assert!(result[0].files.contains(&"b".to_string()));
    }

    #[test]
    fn collapse_codex_duplicate_exchanges_replaces_no_response() {
        let ex1 = make_exchange(1, "user", "(no response)", vec!["a"]);
        let ex2 = make_exchange(2, "user", "hello", vec!["b"]);
        let result = collapse_codex_duplicate_exchanges(vec![ex1, ex2]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].action, "hello");
        assert!(result[0].files.contains(&"a".to_string()));
        assert!(result[0].files.contains(&"b".to_string()));
    }

    #[test]
    fn collapse_codex_duplicate_exchanges_different_users_not_merged() {
        let ex1 = make_exchange(1, "user1", "hello", vec![]);
        let ex2 = make_exchange(2, "user2", "hello", vec![]);
        let result = collapse_codex_duplicate_exchanges(vec![ex1, ex2]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].position, 1);
        assert_eq!(result[1].position, 2);
    }

    // ── format_structured_patch ────────────────────────────────────────────

    #[test]
    fn format_structured_patch_empty() {
        assert_eq!(format_structured_patch(&[]), "");
    }

    #[test]
    fn format_structured_patch_with_hunk() {
        let patch = vec![json!({
            "oldStart": 1,
            "newStart": 2,
            "lines": ["-old line", "+new line", " context"]
        })];
        let result = format_structured_patch(&patch);
        assert_eq!(result, "@@ -1 +2 @@\n-old line\n+new line\n context");
    }
}
