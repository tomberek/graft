use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenario.nix")
}

fn nix_build(attr: &str) -> String {
    let output = Command::new("nix-build")
        .arg(fixture_path())
        .args(["-A", attr, "--no-out-link"])
        .output()
        .expect("failed to run nix-build");
    assert!(
        output.status.success(),
        "nix-build -A {attr} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The builder args of `path`'s deriver — used to tell a real rebuild apart
/// from a graft by checking which recipe actually ran.
fn deriver_args(path: &str) -> String {
    let drv = {
        let out = Command::new("nix-store").args(["-q", "--deriver", path]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let shown = Command::new("nix").args(["derivation", "show", &drv]).output().unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("valid derivation JSON");
    let inner = parsed["derivations"].as_object().unwrap().values().next().unwrap();
    inner["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ")
}

fn nix_store_references(path: &str) -> Vec<String> {
    let output = Command::new("nix-store")
        .args(["-q", "--references", path])
        .output()
        .expect("failed to run nix-store -q --references");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Write an executable shell script to a temp file (kept alive in
/// `keep_alive` for the `graft` invocation that uses it as `$EDITOR`).
fn stub_editor(body: &str, keep_alive: &mut Vec<tempfile::TempPath>) -> PathBuf {
    let file = tempfile::NamedTempFile::new().expect("failed to create stub editor file");
    fs::write(file.path(), format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
    fs::set_permissions(file.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let path = file.path().to_path_buf();
    keep_alive.push(file.into_temp_path());
    path
}

fn graft(args: &[&str], editor: Option<&Path>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_graft"));
    cmd.args(args);
    if let Some(e) = editor {
        cmd.env("EDITOR", e);
    }
    cmd.output().expect("failed to run graft")
}

#[test]
fn replace_grafts_a_consumer_without_rebuilding_it() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let consumer = nix_build("consumer");

    let output = graft(
        &["replace", &consumer, "--replace", &format!("{old}={new}")],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer, "grafted output should be a new store path");

    let run = Command::new(format!("{grafted}/bin/consumer"))
        .output()
        .expect("failed to run grafted consumer");
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "got new dependency");

    let run_orig = Command::new(format!("{consumer}/bin/consumer"))
        .output()
        .expect("failed to run original consumer");
    assert_eq!(String::from_utf8_lossy(&run_orig.stdout).trim(), "got old dependency");

    // References must be registered, not just the content rewritten.
    let refs = nix_store_references(&grafted);
    assert!(refs.contains(&new), "grafted consumer should reference the new dependency: {refs:?}");
    assert!(!refs.contains(&old), "grafted consumer should not reference the old dependency: {refs:?}");
}

#[test]
fn replace_rejects_mismatched_length_basenames_before_touching_the_store() {
    let old = nix_build("oldDep");
    let consumer = nix_build("consumer");
    let bogus_new = format!("{old}-longer-name");

    let output = graft(
        &["replace", &consumer, "--replace", &format!("{old}={bogus_new}")],
        None,
    );
    assert!(!output.status.success(), "replace should reject a length-mismatched pair");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("differ in length"),
        "expected a length-mismatch error, got: {stderr}"
    );
}

#[test]
fn rebuild_mode_does_a_real_rebuild_and_ignores_the_length_constraint() {
    let old = nix_build("oldDepLong");
    let new = nix_build("newDepLong");
    let consumer = nix_build("consumerLong");
    assert_ne!(
        old.len(),
        new.len(),
        "fixture invariant: oldDepLong/newDepLong must have different-length basenames"
    );

    let graft_attempt = graft(&["replace", &consumer, "--replace", &format!("{old}={new}")], None);
    assert!(!graft_attempt.status.success(), "graft mode should still reject this pair");

    let output = graft(
        &["replace", &consumer, "--replace", &format!("{old}={new}"), "--rebuild"],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --rebuild failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rebuilt = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(rebuilt, consumer);

    let run = Command::new(format!("{rebuilt}/bin/consumer-long")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "got new long-named dependency"
    );

    let run_orig = Command::new(format!("{consumer}/bin/consumer-long")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run_orig.stdout).trim(),
        "got old long-named dependency"
    );

    let deriver = {
        let out = Command::new("nix-store").args(["-q", "--deriver", &rebuilt]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let shown = Command::new("nix").args(["derivation", "show", &deriver]).output().unwrap();
    let shown_str = String::from_utf8_lossy(&shown.stdout);
    assert!(
        shown_str.contains("default-builder.sh"),
        "expected the real stdenv builder, got: {shown_str}"
    );
}

#[test]
fn edit_file_grafts_a_hand_edited_file_up_through_the_closure() {
    let consumer = nix_build("consumer");
    let mut editors = Vec::new();
    let editor = stub_editor(r#"echo "edited-marker" >> "$1""#, &mut editors);

    let output = graft(
        &["edit", "file", &consumer, &consumer, "bin/consumer"],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft edit file failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    let grafted_contents = fs::read_to_string(format!("{grafted}/bin/consumer")).unwrap();
    assert!(grafted_contents.contains("edited-marker"), "grafted file should contain the editor's marker");

    let original_contents = fs::read_to_string(format!("{consumer}/bin/consumer")).unwrap();
    assert!(!original_contents.contains("edited-marker"), "original file must be untouched");
}

#[test]
fn edit_drv_resolves_a_plain_output_path_to_its_deriver_automatically() {
    let consumer = nix_build("consumer");
    // Pass the plain output path, not its `.drv` — `edit drv` must resolve
    // the deriver itself.
    let mut editors = Vec::new();
    let editor = stub_editor(
        r#"python3 -c "
import json, sys
path = sys.argv[1]
d = json.load(open(path))
lines = d['env']['text'].splitlines()
d['env']['text'] = lines[0] + '\necho edited-marker\n' + '\n'.join(lines[1:]) + '\n'
json.dump(d, open(path, 'w'))
" "$1""#,
        &mut editors,
    );

    let output = graft(&["edit", "drv", &consumer, &consumer], Some(&editor));
    assert!(
        output.status.success(),
        "graft edit drv failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    let run = Command::new(format!("{grafted}/bin/consumer")).output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("edited-marker"), "rebuilt leaf's marker should show up: {stdout}");

    let run_orig = Command::new(format!("{consumer}/bin/consumer")).output().unwrap();
    assert!(!String::from_utf8_lossy(&run_orig.stdout).contains("edited-marker"), "original must be untouched");
}

#[test]
fn cutoff_stops_propagation_and_leaves_everything_above_it_untouched() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--cutoff", &mid],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --cutoff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    assert_eq!(result, top, "cutting off `mid` should leave `top` completely unchanged");

    let run = Command::new(format!("{top}/bin/top")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "leaf v1", "top must still run the original leaf");
}

#[test]
fn force_rebuild_uses_rebuild_strategy_for_one_path_while_default_stays_graft() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--force-rebuild", &mid],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --force-rebuild failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new_top = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(new_top, top);

    let run = Command::new(format!("{new_top}/bin/top")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "leaf v2");

    let new_mid = nix_store_references(&new_top)
        .into_iter()
        .find(|r| r.ends_with("-mid"))
        .expect("top should reference the rebuilt mid");

    assert!(
        deriver_args(&new_mid).contains("default-builder.sh"),
        "force-rebuilt `mid` should use the real stdenv builder, not the graft recipe"
    );
    let top_args = deriver_args(&new_top);
    assert!(top_args.contains("--dump") && top_args.contains("--restore"), "top should still use the graft recipe: {top_args}");
}

#[test]
fn rebuild_fails_loudly_when_a_reference_is_only_embedded_transitively() {
    // `transitiveTop`'s own .drv never mentions `transitiveLeaf` — only
    // `transitiveWrapper`'s content embeds it as text — so it's a genuine
    // runtime reference with no `.drv`-structural location for --rebuild to find.
    let leaf = nix_build("transitiveLeaf");
    let leaf_v2 = nix_build("transitiveLeafV2");
    let top = nix_build("transitiveTop");

    let rebuild_attempt = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--rebuild"],
        None,
    );
    assert!(!rebuild_attempt.status.success(), "--rebuild should fail loudly, not silently no-op");
    let stderr = String::from_utf8_lossy(&rebuild_attempt.stderr);
    assert!(
        stderr.contains("does not appear anywhere in its derivation"),
        "expected the new unlocatable-substitution error, got: {stderr}"
    );

    // The same replacement succeeds under the default graft strategy, which
    // operates on realized bytes rather than declared structure.
    let graft_output = graft(&["replace", &top, "--replace", &format!("{leaf}={leaf_v2}")], None);
    assert!(
        graft_output.status.success(),
        "graft mode should succeed on the same fixture: {}",
        String::from_utf8_lossy(&graft_output.stderr)
    );
    let grafted = String::from_utf8_lossy(&graft_output.stdout).trim().to_string();
    assert_ne!(grafted, top);
    let contents = fs::read_to_string(format!("{grafted}/wrapper")).unwrap();
    assert!(contents.contains(&leaf_v2), "grafted wrapper should point at the new leaf: {contents}");
}

#[test]
fn rebuild_supports_a_multi_output_derivation() {
    // `multiOut` produces two outputs (`out`, `extra`); `multiConsumer`
    // depends specifically on `extra`.
    let dep = nix_build("multiDep");
    let dep_v2 = nix_build("multiDepV2");
    let consumer = nix_build("multiConsumer");

    let output = graft(&["replace", &consumer, "--replace", &format!("{dep}={dep_v2}"), "--rebuild"], None);
    assert!(
        output.status.success(),
        "graft replace --rebuild failed on a multi-output derivation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new_consumer = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(new_consumer, consumer);

    let run = Command::new(format!("{new_consumer}/bin/multi-consumer")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "multidep v2");
    let run_orig = Command::new(format!("{consumer}/bin/multi-consumer")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run_orig.stdout).trim(), "multidep v1");

    // The rebuilt consumer's inputs.drvs should reference the new `multiOut`
    // by its *specific output name* (`extra`), not just any output.
    let new_multi_extra = nix_store_references(&new_consumer)
        .into_iter()
        .find(|r| r.ends_with("-multi-extra"))
        .expect("consumer should reference the rebuilt multi-output derivation's `extra` output");
    let deriver = {
        let out = Command::new("nix-store").args(["-q", "--deriver", &new_multi_extra]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let shown = Command::new("nix").args(["derivation", "show", &deriver]).output().unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let inner = parsed["derivations"].as_object().unwrap().values().next().unwrap();
    let outputs: Vec<&str> = inner["outputs"].as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert!(outputs.contains(&"out") && outputs.contains(&"extra"), "rebuilt derivation should still have both outputs: {outputs:?}");
}

#[test]
fn interactive_lets_a_single_line_override_the_default_strategy() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let top = nix_build("chainTop");

    // Rewrite whichever line ends in `-mid` to `cutoff`, leaving the rest untouched.
    let mut editors = Vec::new();
    let editor = stub_editor(
        r#"python3 -c "
import sys
path = sys.argv[1]
lines = open(path).read().splitlines()
out = [('cutoff ' + l.split(None, 1)[1]) if l.strip().endswith('-mid') else l for l in lines]
open(path, 'w').write('\n'.join(out) + '\n')
" "$1""#,
        &mut editors,
    );

    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--interactive"],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --interactive failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    assert_eq!(result, top, "interactively cutting off `mid` should leave `top` unchanged, just like --cutoff mid");

    let run = Command::new(format!("{top}/bin/top")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "leaf v1", "top must still run the original leaf");
}

#[test]
fn interactive_accepts_a_one_letter_strategy_shortcut() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let top = nix_build("chainTop");

    // Same scenario as the full-word cutoff test, but using the `c` shortcut.
    let mut editors = Vec::new();
    let editor = stub_editor(
        r#"python3 -c "
import sys
path = sys.argv[1]
lines = open(path).read().splitlines()
out = [('c ' + l.split(None, 1)[1]) if l.strip().endswith('-mid') else l for l in lines]
open(path, 'w').write('\n'.join(out) + '\n')
" "$1""#,
        &mut editors,
    );

    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--interactive"],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --interactive with a one-letter strategy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(result, top, "`c` should behave exactly like `cutoff`, leaving `top` unchanged");
}

#[test]
fn interactive_rejects_added_or_deleted_lines() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let top = nix_build("chainTop");

    let mut editors = Vec::new();
    let delete_a_line = stub_editor(
        r#"python3 -c "
import sys
path = sys.argv[1]
lines = [l for l in open(path).read().splitlines() if l.strip() and not l.strip().startswith('#')]
open(path, 'w').write('\n'.join(lines[:-1]) + '\n')
" "$1""#,
        &mut editors,
    );
    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--interactive"],
        Some(&delete_a_line),
    );
    assert!(!output.status.success(), "deleting a line from the interactive todo should be rejected");
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not be deleted"));
}

#[test]
fn dry_run_detects_rebuild_infeasibility_upfront_without_building_anything() {
    let leaf = nix_build("transitiveLeaf");
    let leaf_v2 = nix_build("transitiveLeafV2");
    let top = nix_build("transitiveTop");

    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--rebuild", "--dry-run"],
        None,
    );
    assert!(
        output.status.success(),
        "--dry-run should never fail just because --rebuild would be infeasible: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("would attempt --rebuild") && stderr.contains("not declared in its own .drv"),
        "expected the rebuild-infeasibility warning, got: {stderr}"
    );

    assert!(
        stderr.contains("is an explicit replacement target"),
        "expected the explicit-target line, got: {stderr}"
    );

    // `wrapper` is plain text, not executable, so read it rather than run it.
    let contents = fs::read_to_string(format!("{top}/wrapper")).unwrap();
    assert!(contents.contains(&leaf), "top's wrapper should still reference the original leaf: {contents}");
}

#[test]
fn interactive_rejects_explicit_rebuild_selection_on_an_infeasible_line() {
    let leaf = nix_build("transitiveLeaf");
    let leaf_v2 = nix_build("transitiveLeafV2");
    let top = nix_build("transitiveTop");

    // The todo defaults this line to `graft`; force it to `rebuild` anyway.
    let mut editors = Vec::new();
    let force_rebuild = stub_editor(
        r#"python3 -c "
import sys
path = sys.argv[1]
lines = open(path).read().splitlines()
out = []
for l in lines:
    if l.strip().startswith('graft') and '-transitive-top' in l:
        out.append('rebuild ' + l.split(None, 1)[1])
    else:
        out.append(l)
open(path, 'w').write('\n'.join(out) + '\n')
" "$1""#,
        &mut editors,
    );

    let output = graft(
        &["replace", &top, "--replace", &format!("{leaf}={leaf_v2}"), "--rebuild", "--interactive"],
        Some(&force_rebuild),
    );
    assert!(!output.status.success(), "selecting `rebuild` on a known-infeasible line should be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot select `rebuild`") && stderr.contains("not declared in its own .drv"),
        "expected the rebuild-rejection error, got: {stderr}"
    );
}
