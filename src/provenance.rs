use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Appends one line to `<out_link>.graft-history.jsonl` recording the exact
/// command that produced this out-link and when. `--out-link` itself only
/// ever points at the latest generation — this is the only way to answer
/// "what did I graft into this result last week" after the fact.
///
/// Timestamps are raw Unix seconds, not a formatted date: adding a
/// date-formatting dependency just for this would be a lot of dependency
/// for one cosmetic improvement. `date -d @<timestamp>` (or `jq`, if
/// scripting against the file) converts it trivially.
pub fn record(out_link: &Path, new_root: &Path) -> Result<()> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let entry = serde_json::json!({
        "timestamp": timestamp,
        "command": std::env::args().collect::<Vec<_>>(),
        "new_root": new_root.display().to_string(),
    });
    let history_path = history_path_for(out_link);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
        .with_context(|| format!("failed to open {} for appending", history_path.display()))?;
    writeln!(file, "{entry}").with_context(|| format!("failed to write to {}", history_path.display()))
}

fn history_path_for(out_link: &Path) -> PathBuf {
    let mut name = out_link.as_os_str().to_os_string();
    name.push(".graft-history.jsonl");
    PathBuf::from(name)
}
