use serde_json::json;

use crate::db::HcomDb;

/// Log a batch launch completion event and trigger a relay push.
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_batch_completion(
    db: &HcomDb,
    launcher_name: &str,
    batch_id: &str,
    base_tool: &str,
    count_requested: usize,
    launched: usize,
    failed: usize,
    background: bool,
    tag: &str,
    handles: &[serde_json::Value],
) {
    // Log batch launch event
    db.log_event(
        "life",
        launcher_name,
        &json!({
            "action": "batch_launched",
            "by": launcher_name,
            "batch_id": batch_id,
            "tool": base_tool,
            "count_requested": count_requested,
            "launched": launched,
            "failed": failed,
            "background": background,
            "tag": tag,
            "instances": handles
                .iter()
                .filter_map(|h| h.get("instance_name").and_then(|v| v.as_str()))
                .collect::<Vec<_>>(),
        }),
    )
    .ok();

    // Push launch event to relay (best-effort)
    let prefix = crate::runtime_env::get_hcom_prefix();
    if let Some((cmd, prefix_args)) = prefix.split_first() {
        let _ = std::process::Command::new(cmd)
            .args(prefix_args)
            .args(["relay", "push"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
