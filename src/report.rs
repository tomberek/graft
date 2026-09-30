use crate::store;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// One node worth showing in a report: something that changed, or caused a
/// change, as opposed to the (usually much larger) unchanged/cutoff part of
/// the closure nobody needs to look at. `depends_on` is pre-filtered by the
/// caller to only the direct references that are *also* in the report, so
/// this module never needs to know about `Category`/the full node set.
pub struct ReportNode {
    pub path: PathBuf,
    pub label: &'static str,
    pub color: &'static str,
    pub level: usize,
    /// `None` under `--dry-run` for anything not yet built (a graft/rebuild
    /// target's real output isn't known until it's actually built).
    pub new_path: Option<PathBuf>,
    pub depends_on: Vec<PathBuf>,
    /// `--report-diff`'s plain-text `nix-diff` output, if requested and
    /// applicable — only for `Explicit`/`NeedsRebuild` nodes, since
    /// grafting never changes the derivation there's nothing to diff.
    pub nix_diff: Option<String>,
    /// `--report-diff`'s `diffoscope --html` report, if requested and a
    /// built new path was available — a filename relative to this same
    /// report directory, not a full path (diffoscope's own output lives
    /// alongside `index.html`).
    pub diffoscope_html: Option<String>,
}

/// Writes `<dir>/index.html`: a self-contained (no external assets, no
/// CDN, no build step) static report — a dependency graph laid out by
/// level (reusing exactly the scheduling levels `replace()` already
/// computes for parallel batching, since that's already the right shape
/// for "what could happen at once") plus a details table. Returns the
/// written file's path.
pub fn write(dir: &Path, closure_root: &Path, new_root: &Path, dry_run: bool, summary: &str, nodes: &[ReportNode]) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create report directory {}", dir.display()))?;
    let index = dir.join("index.html");
    fs::write(&index, render(closure_root, new_root, dry_run, summary, nodes))
        .with_context(|| format!("failed to write {}", index.display()))?;
    Ok(index)
}

fn short_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Just the `name-version` part, stripping the 32-character hash — the
/// graph would be unreadable with `short_name`'s full basename under every
/// node; the table below still shows full paths for anyone who needs them.
fn graph_label(p: &Path) -> String {
    store::store_name(p).unwrap_or_else(|_| short_name(p))
}

fn render(closure_root: &Path, new_root: &Path, dry_run: bool, summary: &str, nodes: &[ReportNode]) -> String {
    const ROW_HEIGHT: u32 = 90;
    const COL_WIDTH: u32 = 170;
    const NODE_RADIUS: u32 = 28;

    let max_level = nodes.iter().map(|n| n.level).max().unwrap_or(0);
    let mut by_level: Vec<Vec<&ReportNode>> = (0..=max_level).map(|_| Vec::new()).collect();
    for n in nodes {
        by_level[n.level].push(n);
    }
    let widest_row = by_level.iter().map(Vec::len).max().unwrap_or(1).max(1) as u32;
    let width = widest_row * COL_WIDTH + COL_WIDTH;
    let height = (max_level as u32 + 1) * ROW_HEIGHT + ROW_HEIGHT;

    let mut pos: HashMap<&Path, (u32, u32)> = HashMap::new();
    for (level, row) in by_level.iter().enumerate() {
        let n = row.len() as u32;
        for (i, node) in row.iter().enumerate() {
            let x = (i as u32 + 1) * (width / (n + 1));
            let y = height - (level as u32 + 1) * ROW_HEIGHT;
            pos.insert(node.path.as_path(), (x, y));
        }
    }

    let mut svg_edges = String::new();
    let mut svg_nodes = String::new();
    for node in nodes {
        let (x, y) = pos[node.path.as_path()];
        for dep in &node.depends_on {
            if let Some(&(dx, dy)) = pos.get(dep.as_path()) {
                let _ = writeln!(
                    svg_edges,
                    r##"<line x1="{x}" y1="{y}" x2="{dx}" y2="{dy}" stroke="#aaa" stroke-width="1.5" marker-end="url(#arrow)"/>"##
                );
            }
        }
        let tooltip = format!(
            "{} -> {}",
            node.path.display(),
            node.new_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(pending)".to_string())
        );
        let _ = writeln!(
            svg_nodes,
            r##"<g class="node"><circle cx="{x}" cy="{y}" r="{NODE_RADIUS}" fill="{}"><title>{}</title></circle>
               <text x="{x}" y="{}" text-anchor="middle">{}</text></g>"##,
            node.color,
            escape(&tooltip),
            y + NODE_RADIUS + 16,
            escape(&graph_label(&node.path)),
        );
    }

    let mut rows_html = String::new();
    for n in nodes {
        let mut diff_cell = String::new();
        if let Some(text) = &n.nix_diff {
            let _ = write!(diff_cell, "<details><summary>nix-diff</summary><pre>{}</pre></details>", escape(text));
        }
        if let Some(file) = &n.diffoscope_html {
            let _ = write!(diff_cell, r#"<a href="{}" target="_blank">diffoscope</a>"#, escape(file));
        }
        let _ = writeln!(
            rows_html,
            "<tr><td>{}</td><td><span class=\"tag\" style=\"background:{}\">{}</span></td>\
             <td class=\"mono\">{}</td><td class=\"mono\">{}</td><td>{}</td></tr>",
            escape(&short_name(&n.path)),
            n.color,
            n.label,
            escape(&n.path.display().to_string()),
            n.new_path.as_ref().map(|p| escape(&p.display().to_string())).unwrap_or_else(|| "(pending)".to_string()),
            diff_cell,
        );
    }

    format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>graft report</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; margin: 2rem; color: #1a1a1a; }}
  h1 {{ font-size: 1.25rem; margin-bottom: 0.25rem; }}
  .mono {{ font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 0.82rem; }}
  table {{ border-collapse: collapse; width: 100%; margin-top: 1.5rem; }}
  td, th {{ border-bottom: 1px solid #e5e5e5; padding: 0.4rem 0.6rem; text-align: left; }}
  th {{ color: #666; font-weight: 600; font-size: 0.85rem; }}
  .tag {{ color: white; border-radius: 4px; padding: 0.15rem 0.55rem; font-size: 0.78rem; white-space: nowrap; }}
  svg {{ background: #fafafa; border: 1px solid #eee; border-radius: 8px; margin-top: 1rem; }}
  .node text {{ fill: #333; font-weight: 600; font-size: 11px; pointer-events: none; }}
  .node circle {{ stroke: rgba(0,0,0,0.15); stroke-width: 1; }}
  pre {{ background: #f5f5f5; padding: 0.6rem; border-radius: 6px; overflow-x: auto; font-size: 0.78rem; }}
  details summary {{ cursor: pointer; color: #2563eb; font-size: 0.82rem; }}
  td a {{ color: #2563eb; font-size: 0.82rem; margin-left: 0.5rem; }}
</style></head>
<body>
<h1>graft report{}</h1>
<p class="mono">{} &rarr; {}</p>
<p>{}</p>
<svg width="{width}" height="{height}">
  <defs><marker id="arrow" markerWidth="8" markerHeight="8" refX="6" refY="3" orient="auto">
    <path d="M0,0 L6,3 L0,6 Z" fill="#aaa"/></marker></defs>
  {svg_edges}
  {svg_nodes}
</svg>
<table>
  <tr><th>path</th><th>strategy</th><th>old</th><th>new</th><th>diff</th></tr>
  {rows_html}
</table>
</body></html>"##,
        if dry_run { " (dry run)" } else { "" },
        escape(&closure_root.display().to_string()),
        escape(&new_root.display().to_string()),
        escape(summary),
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
