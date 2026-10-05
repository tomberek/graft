use crate::{derivation, editor, log, store};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Dump `path` (which must be in `closure`), open `$EDITOR` on `subpath`
/// inside it (or the whole extracted tree if omitted), and return the
/// `(old, new)` pair. Grafting it up through the closure is the caller's
/// job (`main.rs`'s `collect_pairs`) — several of these, and/or
/// `--override`/`--edit`, can be combined into one closure walk, so
/// producing the pair is kept separate from applying it.
///
/// The edited tree is staged into the store via `nix store add` first —
/// a sandboxed build can only see declared store-path inputs, never an
/// arbitrary host directory, so there's no way to feed the edit straight
/// into a derivation builder without a store path to point it at — then
/// re-added a *second* time through a synthetic derivation (the same
/// dump/restore recipe `replace::graft_recipe` uses). `nix store add`
/// alone never registers references at all (see DESIGN.md §4/§5): the
/// second pass exists purely so the daemon's post-build scan has a real
/// build to register them against. That scan only ever matches a build's
/// *declared* inputs, never an unbounded search of the whole store, so
/// `store::scan_references` — a byte-level scan of the edited tree for
/// anything that looks like `/nix/store/<hash>-<name>` — is what supplies
/// the right candidate set as `inputSrcs`, the same way `graft_recipe`
/// declares every direct reference (changed or not) of what it's grafting.
pub fn produce_pair(
    closure_root: &Path,
    closure: &[PathBuf],
    path: &Path,
    subpath: Option<&Path>,
    nix_args: &[String],
) -> Result<(PathBuf, PathBuf)> {
    store::require_output_path(path)?;
    if !closure.iter().any(|p| p == path) {
        bail!(
            "{} is not in the closure of {}",
            path.display(),
            closure_root.display()
        );
    }

    let name = store::store_name(path)?;
    log::v(format!("dumping {} to extract for editing", path.display()));
    let nar = store::dump(path)?;
    let tmp = tempfile::tempdir().context("failed to create scratch dir")?;
    let extracted = tmp.path().join(&name);
    store::restore(&nar, &extracted)?;
    log::v(format!("extracted to {}", extracted.display()));

    let edit_target = match subpath {
        Some(sp) => extracted.join(sp),
        None => extracted.clone(),
    };
    if !edit_target.exists() {
        bail!(
            "{} does not exist inside {}",
            edit_target.display(),
            path.display()
        );
    }
    log::v(format!("opening $EDITOR on {}", edit_target.display()));
    editor::edit(&edit_target)?;

    log::v(format!(
        "staging edited {} into the store",
        extracted.display()
    ));
    let staged = store::add_fixed_recursive(&extracted)?;
    let refs = store::scan_references(&extracted)?;
    log::v(format!(
        "found {} embedded reference(s) in the edited tree",
        refs.len()
    ));

    let bash = derivation::tool_path("bash")?;
    let nix_store = derivation::tool_path("nix-store")?;
    let script = format!(
        "\"{ns}\" --dump \"{staged}\" | \"{ns}\" --restore \"$out\"",
        ns = nix_store.display(),
        staged = staged.display(),
    );
    let mut input_srcs: Vec<PathBuf> = vec![
        staged.clone(),
        store::store_root(&bash)?,
        store::store_root(&nix_store)?,
    ];
    input_srcs.extend(refs);
    input_srcs.sort();
    input_srcs.dedup();

    let new_drv = derivation::construct_derivation(derivation::DerivationSpec {
        name,
        builder: bash,
        args: vec!["-c".to_string(), script],
        input_srcs,
    })?;
    let new_path = derivation::realise(&new_drv, "out", nix_args)?;
    log::v(format!(
        "edited path: {} -> {}",
        path.display(),
        new_path.display()
    ));
    Ok((path.to_path_buf(), new_path))
}
