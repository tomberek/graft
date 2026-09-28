use anyhow::{bail, Context, Result};
use std::env;
use std::path::Path;
use std::process::Command;

/// Launch `$EDITOR` (falling back to `vi`) on `target` and block until it
/// exits. `$EDITOR` is split on whitespace (e.g. `"code --wait"`), as git does.
pub fn edit(target: &Path) -> Result<()> {
    let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = Command::new(program)
        .args(parts)
        .arg(target)
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` exited with a failure status");
    }
    Ok(())
}
