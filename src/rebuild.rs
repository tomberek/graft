use crate::derivation::OutputTarget;
use crate::{derivation, log, store};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The `--rebuild` counterpart to `replace::graft_recipe`: find `path`'s
/// deriver, substitute the changed dependencies in its derivation JSON, and
/// return the new `.drv` path plus its output name — building it is a
/// separate step left to the caller, so several independent rebuilds can be
/// batched into one `nix build` call (see `derivation::build_many`).
/// Requires a known deriver — a graft or directly-added path has none.
///
/// Locates the substitution point via `path`'s declared `.drv` structure, a
/// different graph than the runtime reference graph that decided `path`
/// needs touching in the first place (see DESIGN.md §6) — when a changed
/// reference isn't declared there, this fails loudly rather than silently
/// rebuilding unmodified.
pub fn rebuild_recipe(path: &Path, all_refs: &[(PathBuf, PathBuf)]) -> Result<(PathBuf, String)> {
    let changed: Vec<&(PathBuf, PathBuf)> = all_refs.iter().filter(|(o, n)| o != n).collect();

    log::v(format!("rebuilding {} ({} changed dependency/ies)", path.display(), changed.len()));
    let deriver = derivation::deriver_of(path).with_context(|| format!("cannot rebuild {}", path.display()))?;
    let shown = derivation::show(&deriver)?;
    let (output_name, _output_path, mut inner) = derivation::locate_output(&shown, &deriver, Some(OutputTarget::Path(path)))?;

    for (old, new) in &changed {
        log::v(format!(
            "substituting dependency in {}'s derivation: {} -> {}",
            path.display(),
            old.display(),
            new.display()
        ));
        let found = substitute_dependency(&mut inner, old, new)?;
        if !found {
            bail!(
                "cannot rebuild {}: {} does not appear anywhere in its derivation (checked \
                 inputs.drvs, inputs.srcs, env, and args) — the .drv-structural graph and the \
                 runtime reference graph disagree here, most likely because the reference is only \
                 embedded transitively through some other declared input. Pass --cutoff {} to \
                 skip this path, or accept the default graft strategy for it instead (grafting \
                 operates on realized output bytes, not declared structure, so it doesn't have \
                 this gap).",
                path.display(),
                old.display(),
                path.display(),
            );
        }
    }

    let new_drv = derivation::add_with_retry(inner)?;
    Ok((new_drv, output_name))
}

/// Which of `changed_old_refs` a `--rebuild` of `path` would fail to locate,
/// checked without building or mutating anything (safe to call during
/// `--dry-run`/`--interactive` discovery). `Ok(None)` means `path` has no
/// deriver at all; `Ok(Some(missing))` lists which refs couldn't be found
/// (empty means fully feasible).
///
/// Reuses `substitute_dependency` with `old` as both the old and new value
/// (a no-op probe) rather than a separate check that could drift out of
/// sync with what a real rebuild would find.
pub fn unlocatable_dependencies(path: &Path, changed_old_refs: &[PathBuf]) -> Result<Option<Vec<PathBuf>>> {
    let deriver = match derivation::deriver_of(path) {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    let shown = derivation::show(&deriver)?;
    let (_, _, inner) = derivation::locate_output(&shown, &deriver, Some(OutputTarget::Path(path)))?;

    let mut missing = Vec::new();
    for old in changed_old_refs {
        log::v(format!("checking whether --rebuild could locate {} in {}'s derivation", old.display(), path.display()));
        let mut probe = inner.clone();
        if !substitute_dependency(&mut probe, old, old)? {
            missing.push(old.clone());
        }
    }
    Ok(Some(missing))
}

/// `old`'s own deriver and output name, if it has one — `None` for anything
/// without a deriver (a graft, or content added directly to the store).
fn own_output(path: &Path) -> Option<(PathBuf, String)> {
    let drv = derivation::deriver_of(path).ok()?;
    let shown = derivation::show(&drv).ok()?;
    let (name, _, _) = derivation::locate_output(&shown, &drv, Some(OutputTarget::Path(path))).ok()?;
    Some((drv, name))
}

/// Swap every reference to `old` in a derivation's JSON for `new`. Returns
/// whether it found and changed anything; the caller treats "found nothing"
/// as a hard failure rather than proceeding with an unmodified derivation.
///
/// Checks all of the following, not just the first match:
/// 1. `inputs.drvs`: removes just `old`'s output name from its entry's
///    `outputs` list (a derivation can depend on more than one output of
///    the same input), dropping the entry if it becomes empty. Adds `new`'s
///    output name to its own entry, or to `inputs.srcs` if it has no deriver.
/// 2. `inputs.srcs`: a plain-store-path entry matching `old`'s basename.
/// 3. `env`/`args` strings containing `old`'s full path or basename.
fn substitute_dependency(inner: &mut Value, old: &Path, new: &Path) -> Result<bool> {
    let old_base = store::basename(old)?;
    let new_base = store::basename(new)?;
    let old_full = old.to_string_lossy().into_owned();
    let new_full = new.to_string_lossy().into_owned();
    let mut found = false;

    if let Some((old_drv, old_output_name)) = own_output(old) {
        let old_drv_base = store::basename(&old_drv)?;
        let has_entry = inner
            .get("inputs")
            .and_then(|i| i.get("drvs"))
            .and_then(|d| d.get(&old_drv_base))
            .is_some();
        if has_entry {
            let drvs = inner["inputs"]["drvs"].as_object_mut().context("derivation has no `inputs.drvs`")?;
            let mut drop_entry = false;
            if let Some(entry) = drvs.get_mut(&old_drv_base) {
                if let Some(outputs_list) = entry.get_mut("outputs").and_then(|o| o.as_array_mut()) {
                    let before = outputs_list.len();
                    outputs_list.retain(|v| v.as_str() != Some(old_output_name.as_str()));
                    if outputs_list.len() < before {
                        found = true;
                    }
                    drop_entry = outputs_list.is_empty();
                }
            }
            if drop_entry {
                drvs.remove(&old_drv_base);
            }

            if found {
                match own_output(new) {
                    Some((new_drv, new_output_name)) => {
                        let new_drv_base = store::basename(&new_drv)?;
                        let drvs = inner["inputs"]["drvs"].as_object_mut().unwrap();
                        let entry = drvs
                            .entry(new_drv_base.clone())
                            .or_insert_with(|| serde_json::json!({ "dynamicOutputs": {}, "outputs": [] }));
                        let outputs_list = entry["outputs"].as_array_mut().context("inputs.drvs entry has no `outputs` array")?;
                        let name_val = Value::String(new_output_name.clone());
                        if !outputs_list.contains(&name_val) {
                            outputs_list.push(name_val);
                        }
                        log::v(format!("  inputs.drvs: {old_drv_base}.{old_output_name} -> {new_drv_base}.{new_output_name}"));
                    }
                    None => {
                        log::v(format!(
                            "  inputs.drvs: removed {old_drv_base}.{old_output_name}, added {new_base} to \
                             inputs.srcs instead ({} has no deriver of its own)",
                            new.display()
                        ));
                        inner["inputs"]["srcs"]
                            .as_array_mut()
                            .context("derivation has no `inputs.srcs` to add to")?
                            .push(Value::String(new_base.clone()));
                    }
                }
            }
        }
    }

    if let Some(srcs) = inner.get_mut("inputs").and_then(|i| i.get_mut("srcs")).and_then(|s| s.as_array_mut()) {
        for v in srcs.iter_mut() {
            if v.as_str() == Some(old_base.as_str()) {
                log::v(format!("  inputs.srcs: {old_base} -> {new_base}"));
                *v = Value::String(new_base.clone());
                found = true;
            }
        }
    }

    if let Some(env) = inner.get_mut("env").and_then(|e| e.as_object_mut()) {
        for (k, v) in env.iter_mut() {
            if let Some(s) = v.as_str() {
                if s.contains(&old_full) || s.contains(old_base.as_str()) {
                    log::v(format!("  env.{k}: substituted {old_base} -> {new_base}"));
                    *v = Value::String(s.replace(&old_full, &new_full).replace(old_base.as_str(), new_base.as_str()));
                    found = true;
                }
            }
        }
    }
    if let Some(args) = inner.get_mut("args").and_then(|a| a.as_array_mut()) {
        for (i, v) in args.iter_mut().enumerate() {
            if let Some(s) = v.as_str() {
                if s.contains(&old_full) {
                    log::v(format!("  args[{i}]: substituted {old_base} -> {new_base}"));
                    *v = Value::String(s.replace(&old_full, &new_full));
                    found = true;
                }
            }
        }
    }

    Ok(found)
}
