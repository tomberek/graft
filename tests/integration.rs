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
        let out = Command::new("nix-store")
            .args(["-q", "--deriver", path])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let shown = Command::new("nix")
        .args(["derivation", "show", &drv])
        .output()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_slice(&shown.stdout).expect("valid derivation JSON");
    let inner = parsed["derivations"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
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

    let output = graft(&["replace", &consumer, "--override", &old, &new], None);
    assert!(
        output.status.success(),
        "graft replace failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(
        grafted, consumer,
        "grafted output should be a new store path"
    );

    let run = Command::new(format!("{grafted}/bin/consumer"))
        .output()
        .expect("failed to run grafted consumer");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "got new dependency"
    );

    let run_orig = Command::new(format!("{consumer}/bin/consumer"))
        .output()
        .expect("failed to run original consumer");
    assert_eq!(
        String::from_utf8_lossy(&run_orig.stdout).trim(),
        "got old dependency"
    );

    // References must be registered, not just the content rewritten.
    let refs = nix_store_references(&grafted);
    assert!(
        refs.contains(&new),
        "grafted consumer should reference the new dependency: {refs:?}"
    );
    assert!(
        !refs.contains(&old),
        "grafted consumer should not reference the old dependency: {refs:?}"
    );
}

#[test]
fn replace_rejects_mismatched_length_basenames_before_touching_the_store() {
    let old = nix_build("oldDep");
    let consumer = nix_build("consumer");
    let bogus_new = format!("{old}-longer-name");

    let output = graft(
        &["replace", &consumer, "--override", &old, &bogus_new],
        None,
    );
    assert!(
        !output.status.success(),
        "replace should reject a length-mismatched pair"
    );
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

    let graft_attempt = graft(&["replace", &consumer, "--override", &old, &new], None);
    assert!(
        !graft_attempt.status.success(),
        "graft mode should still reject this pair"
    );

    let output = graft(
        &["replace", &consumer, "--override", &old, &new, "--rebuild"],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --rebuild failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rebuilt = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(rebuilt, consumer);

    let run = Command::new(format!("{rebuilt}/bin/consumer-long"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "got new long-named dependency"
    );

    let run_orig = Command::new(format!("{consumer}/bin/consumer-long"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run_orig.stdout).trim(),
        "got old long-named dependency"
    );

    let deriver = {
        let out = Command::new("nix-store")
            .args(["-q", "--deriver", &rebuilt])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let shown = Command::new("nix")
        .args(["derivation", "show", &deriver])
        .output()
        .unwrap();
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
        &["replace", &consumer, "--edit", &consumer, "bin/consumer"],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --edit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    let grafted_contents = fs::read_to_string(format!("{grafted}/bin/consumer")).unwrap();
    assert!(
        grafted_contents.contains("edited-marker"),
        "grafted file should contain the editor's marker"
    );

    let original_contents = fs::read_to_string(format!("{consumer}/bin/consumer")).unwrap();
    assert!(
        !original_contents.contains("edited-marker"),
        "original file must be untouched"
    );
}

#[test]
fn edit_drv_resolves_a_plain_output_path_to_its_deriver_automatically() {
    let consumer = nix_build("consumer");
    // Pass the plain output path, not its `.drv`, with its own output
    // name ("out") as the selector — not a subpath that exists inside it,
    // so detect_edit falls back to resolving its deriver automatically.
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

    let output = graft(
        &["replace", &consumer, "--edit", &consumer, "out"],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --edit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    let run = Command::new(format!("{grafted}/bin/consumer"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("edited-marker"),
        "rebuilt leaf's marker should show up: {stdout}"
    );

    let run_orig = Command::new(format!("{consumer}/bin/consumer"))
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&run_orig.stdout).contains("edited-marker"),
        "original must be untouched"
    );
}

#[test]
fn edit_detects_a_nix_file_on_disk_and_rebuilds_the_attribute() {
    // A dedicated scratch file, not the shared scenario.nix fixture --
    // nix-edit opens $EDITOR on the file in place, which would otherwise
    // permanently mutate a fixture every other test also depends on.
    let dir = tempfile::tempdir().unwrap();
    let nix_file = dir.path().join("pkg.nix");
    fs::write(
        &nix_file,
        r#"with import <nixpkgs> {}; writeShellScriptBin "edit-nix-greeter" ''echo "hello v1"''"#,
    )
    .unwrap();

    let orig = Command::new("nix-build")
        .args([nix_file.to_str().unwrap(), "--no-out-link"])
        .output()
        .unwrap();
    assert!(
        orig.status.success(),
        "{}",
        String::from_utf8_lossy(&orig.stderr)
    );
    let orig_path = String::from_utf8_lossy(&orig.stdout).trim().to_string();

    let mut editors = Vec::new();
    let editor = stub_editor(r#"sed -i 's/hello v1/hello v2/' "$1""#, &mut editors);

    let output = graft(
        &[
            "replace",
            &orig_path,
            "--edit",
            nix_file.to_str().unwrap(),
            ".",
        ],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --edit (nix-edit detection) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, orig_path);

    let run = Command::new(format!("{grafted}/bin/edit-nix-greeter"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "hello v2");

    let run_orig = Command::new(format!("{orig_path}/bin/edit-nix-greeter"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run_orig.stdout).trim(),
        "hello v1",
        "original must be untouched"
    );
}

#[test]
fn cutoff_stops_propagation_and_leaves_everything_above_it_untouched() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--cutoff",
            &mid,
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --cutoff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    assert_eq!(
        result, top,
        "cutting off `mid` should leave `top` completely unchanged"
    );

    let run = Command::new(format!("{top}/bin/top")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "leaf v1",
        "top must still run the original leaf"
    );
}

#[test]
fn force_rebuild_uses_rebuild_strategy_for_one_path_while_default_stays_graft() {
    let leaf = nix_build("chainLeaf");
    let leaf_v2 = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--force-rebuild",
            &mid,
        ],
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
    assert!(
        top_args.contains("--dump") && top_args.contains("--restore"),
        "top should still use the graft recipe: {top_args}"
    );
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
        &["replace", &top, "--override", &leaf, &leaf_v2, "--rebuild"],
        None,
    );
    assert!(
        !rebuild_attempt.status.success(),
        "--rebuild should fail loudly, not silently no-op"
    );
    let stderr = String::from_utf8_lossy(&rebuild_attempt.stderr);
    assert!(
        stderr.contains("does not appear anywhere in its derivation"),
        "expected the new unlocatable-substitution error, got: {stderr}"
    );

    // The same replacement succeeds under the default graft strategy, which
    // operates on realized bytes rather than declared structure.
    let graft_output = graft(&["replace", &top, "--override", &leaf, &leaf_v2], None);
    assert!(
        graft_output.status.success(),
        "graft mode should succeed on the same fixture: {}",
        String::from_utf8_lossy(&graft_output.stderr)
    );
    let grafted = String::from_utf8_lossy(&graft_output.stdout)
        .trim()
        .to_string();
    assert_ne!(grafted, top);
    let contents = fs::read_to_string(format!("{grafted}/wrapper")).unwrap();
    assert!(
        contents.contains(&leaf_v2),
        "grafted wrapper should point at the new leaf: {contents}"
    );
}

#[test]
fn rebuild_supports_a_multi_output_derivation() {
    // `multiOut` produces two outputs (`out`, `extra`); `multiConsumer`
    // depends specifically on `extra`.
    let dep = nix_build("multiDep");
    let dep_v2 = nix_build("multiDepV2");
    let consumer = nix_build("multiConsumer");

    let output = graft(
        &[
            "replace",
            &consumer,
            "--override",
            &dep,
            &dep_v2,
            "--rebuild",
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --rebuild failed on a multi-output derivation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new_consumer = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(new_consumer, consumer);

    let run = Command::new(format!("{new_consumer}/bin/multi-consumer"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "multidep v2");
    let run_orig = Command::new(format!("{consumer}/bin/multi-consumer"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run_orig.stdout).trim(),
        "multidep v1"
    );

    // The rebuilt consumer's inputs.drvs should reference the new `multiOut`
    // by its *specific output name* (`extra`), not just any output.
    let new_multi_extra = nix_store_references(&new_consumer)
        .into_iter()
        .find(|r| r.ends_with("-multi-extra"))
        .expect("consumer should reference the rebuilt multi-output derivation's `extra` output");
    let deriver = {
        let out = Command::new("nix-store")
            .args(["-q", "--deriver", &new_multi_extra])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let shown = Command::new("nix")
        .args(["derivation", "show", &deriver])
        .output()
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let inner = parsed["derivations"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    let outputs: Vec<&str> = inner["outputs"]
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    assert!(
        outputs.contains(&"out") && outputs.contains(&"extra"),
        "rebuilt derivation should still have both outputs: {outputs:?}"
    );
}

#[test]
fn override_all_outputs_pairs_every_output_by_name_in_one_shot() {
    // multiAllConsumer genuinely references both outputs of multiAllOld
    // (via symlinks); `old^* new^*` should pair `out` with `out` and
    // `extra` with `extra` without needing two separate --override calls.
    let old_drv = {
        let out = Command::new("nix-instantiate")
            .args([fixture_path().to_str().unwrap(), "-A", "multiAllOld"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let new_drv = {
        let out = Command::new("nix-instantiate")
            .args([fixture_path().to_str().unwrap(), "-A", "multiAllNew"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let consumer = nix_build("multiAllConsumer");

    let output = graft(
        &[
            "replace",
            &consumer,
            "--override",
            &format!("{old_drv}^*"),
            &format!("{new_drv}^*"),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --override old^* new^* failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    let out_target = fs::read_link(format!("{grafted}/out-link")).unwrap();
    let extra_target = fs::read_link(format!("{grafted}/extra-link")).unwrap();
    assert_eq!(
        fs::read_to_string(out_target.join("data")).unwrap().trim(),
        "out v2",
        "the `out` output should have been paired and replaced"
    );
    assert_eq!(
        fs::read_to_string(extra_target.join("data"))
            .unwrap()
            .trim(),
        "extra v2",
        "the `extra` output should have been paired and replaced too"
    );
}

#[test]
fn edit_all_outputs_rebuilds_every_output_from_one_edit_session() {
    // multiOut produces `out` (unrelated to any dependency) and `extra`
    // (depends on multiDep); `--edit <drv>^* .` should edit the recipe
    // once and still produce correctly rebuilt pairs for both outputs.
    let out_path = nix_build("multiOut");
    let drv = {
        let out = Command::new("nix-store")
            .args(["-q", "--deriver", &out_path])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let mut editors = Vec::new();
    let editor = stub_editor(
        r#"python3 -c "
import json, sys
path = sys.argv[1]
d = json.load(open(path))
d['env']['buildCommand'] += '\necho edited-all-outputs >> \$extra/marker\n'
json.dump(d, open(path, 'w'))
" "$1""#,
        &mut editors,
    );

    let output = graft(
        &[
            "-v",
            "replace",
            &out_path,
            "--edit",
            &format!("{drv}^*"),
            ".",
        ],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --edit drv^* failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, out_path);

    let marker = fs::read_to_string(format!("{grafted}/marker")).unwrap_or_default();
    assert!(
        marker.is_empty(),
        "`out` itself wasn't touched by the edit, just rebuilt alongside `extra`: {marker}"
    );

    // `out` and `extra` are independent sibling outputs with no reference
    // between them, so the only way to find the rebuilt `extra` path (to
    // check *it* changed too) is the `-v` log line reporting it directly.
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let new_extra = stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("[graft] rebuilt leaf (extra): ")?
                .split_once(" -> ")
        })
        .map(|(_, new)| new.to_string())
        .unwrap_or_else(|| panic!("expected a 'rebuilt leaf (extra): ...' log line: {stderr}"));
    let extra_marker = fs::read_to_string(format!("{new_extra}/marker")).unwrap();
    assert!(
        extra_marker.contains("edited-all-outputs"),
        "extra output should show the editor's marker: {extra_marker}"
    );
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
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--interactive",
        ],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --interactive failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    assert_eq!(
        result, top,
        "interactively cutting off `mid` should leave `top` unchanged, just like --cutoff mid"
    );

    let run = Command::new(format!("{top}/bin/top")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "leaf v1",
        "top must still run the original leaf"
    );
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
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--interactive",
        ],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "graft replace --interactive with a one-letter strategy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        result, top,
        "`c` should behave exactly like `cutoff`, leaving `top` unchanged"
    );
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
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--interactive",
        ],
        Some(&delete_a_line),
    );
    assert!(
        !output.status.success(),
        "deleting a line from the interactive todo should be rejected"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not be deleted"));
}

#[test]
fn dry_run_detects_rebuild_infeasibility_upfront_without_building_anything() {
    let leaf = nix_build("transitiveLeaf");
    let leaf_v2 = nix_build("transitiveLeafV2");
    let top = nix_build("transitiveTop");

    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--rebuild",
            "--dry-run",
        ],
        None,
    );
    assert!(
        output.status.success(),
        "--dry-run should never fail just because --rebuild would be infeasible: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("would attempt --rebuild")
            && stderr.contains("not declared in its own .drv"),
        "expected the rebuild-infeasibility warning, got: {stderr}"
    );

    assert!(
        stderr.contains("is an explicit replacement target"),
        "expected the explicit-target line, got: {stderr}"
    );

    // `wrapper` is plain text, not executable, so read it rather than run it.
    let contents = fs::read_to_string(format!("{top}/wrapper")).unwrap();
    assert!(
        contents.contains(&leaf),
        "top's wrapper should still reference the original leaf: {contents}"
    );
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
        &[
            "replace",
            &top,
            "--override",
            &leaf,
            &leaf_v2,
            "--rebuild",
            "--interactive",
        ],
        Some(&force_rebuild),
    );
    assert!(
        !output.status.success(),
        "selecting `rebuild` on a known-infeasible line should be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot select `rebuild`")
            && stderr.contains("not declared in its own .drv"),
        "expected the rebuild-rejection error, got: {stderr}"
    );
}

#[test]
fn replace_accepts_file_hash_attr_installables_without_prebuilding() {
    // No `nix_build()` calls here at all — closure-root and both sides of
    // --override are given as installables and must be built by graft itself.
    let fixture = fixture_path().display().to_string();
    let old_inst = format!("{fixture}#oldDep");
    let new_inst = format!("{fixture}#newDep");
    let consumer_inst = format!("{fixture}#consumer");

    let output = graft(
        &[
            "replace",
            &consumer_inst,
            "--override",
            &old_inst,
            &new_inst,
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace with installables failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let run = Command::new(format!("{grafted}/bin/consumer"))
        .output()
        .expect("failed to run grafted consumer");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "got new dependency"
    );
}

#[test]
fn nixos_system_defaults_to_profile_without_switching() {
    let old = nix_build("systemLikeOldDep");
    let new = nix_build("systemLikeNewDep");
    let system = nix_build("systemLike");

    let profile_dir = tempfile::tempdir().unwrap();
    let profile_path = profile_dir.path().join("system");
    std::os::unix::fs::symlink(&system, &profile_path).unwrap();

    let output = graft(
        &[
            "nixos-system",
            "--profile",
            profile_path.to_str().unwrap(),
            "--override",
            &old,
            &new,
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft nixos-system failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, system);

    // Without --switch, nothing should have run switch-to-configuration or
    // touched the profile symlink — just report what to do manually.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not switching") && stderr.contains("switch-to-configuration switch"),
        "expected manual-activation instructions, got: {stderr}"
    );
    assert_eq!(
        fs::read_link(&profile_path).unwrap(),
        PathBuf::from(&system),
        "profile symlink must be untouched"
    );

    let run = Command::new(format!("{grafted}/bin/dependency"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "got new dependency"
    );
}

#[test]
fn nixos_system_switch_registers_the_profile_and_runs_switch_to_configuration() {
    let old = nix_build("systemLikeOldDep");
    let new = nix_build("systemLikeNewDep");
    let system = nix_build("systemLike");

    let profile_dir = tempfile::tempdir().unwrap();
    let profile_path = profile_dir.path().join("system");
    std::os::unix::fs::symlink(&system, &profile_path).unwrap();
    let marker = tempfile::NamedTempFile::new().unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_graft"));
    cmd.args([
        "nixos-system",
        "--profile",
        profile_path.to_str().unwrap(),
        "--override",
        &old,
        &new,
        "--switch",
        "test",
    ]);
    cmd.env("GRAFT_TEST_SWITCH_MARKER", marker.path());
    let output = cmd
        .output()
        .expect("failed to run graft nixos-system --switch");
    assert!(
        output.status.success(),
        "graft nixos-system --switch test failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // switch-to-configuration must have actually run, with the right action...
    let marker_contents =
        fs::read_to_string(marker.path()).expect("switch-to-configuration should have run");
    assert_eq!(marker_contents.trim(), "called with: test");

    // ...and nix-env --set must have repointed the profile at the new generation.
    let resolved = fs::canonicalize(&profile_path).unwrap();
    assert_eq!(resolved, PathBuf::from(&grafted));
}

#[test]
fn replace_grafts_a_real_elf_binarys_embedded_rpath() {
    // Every other fixture is a shell script or plain text; this is the only
    // one exercising actual machine code and NUL bytes, closing a real gap
    // in what the sed-based rewrite had ever been tested against.
    let old_lib = nix_build("binOldLib");
    let new_lib = nix_build("binNewLib");
    let consumer = nix_build("binConsumer");
    assert_eq!(
        old_lib.rsplit('/').next().unwrap().len(),
        new_lib.rsplit('/').next().unwrap().len(),
        "fixture invariant: binOldLib/binNewLib must have equal-length basenames"
    );

    let run_orig = Command::new(format!("{consumer}/bin/consumer"))
        .output()
        .expect("failed to run original consumer");
    assert_eq!(
        String::from_utf8_lossy(&run_orig.stdout).trim(),
        "answer: 1"
    );

    let output = graft(
        &["replace", &consumer, "--override", &old_lib, &new_lib],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace on a real ELF binary failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    // The dynamic linker must actually follow the rewritten RUNPATH/RPATH —
    // this only passes if the ELF bytes were rewritten correctly, not just
    // "didn't crash".
    let run = Command::new(format!("{grafted}/bin/consumer"))
        .output()
        .expect("failed to run grafted consumer");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "answer: 2",
        "grafted binary should dynamically link against the new library and print its answer"
    );

    let refs = nix_store_references(&grafted);
    assert!(
        refs.contains(&new_lib),
        "grafted binary should reference the new library: {refs:?}"
    );
    assert!(
        !refs.contains(&old_lib),
        "grafted binary should not reference the old library: {refs:?}"
    );
}

#[test]
fn replace_rewrites_a_self_reference_to_the_grafted_result_s_own_path() {
    // `selfRefConsumer` embeds its own `$out` in its content, independent
    // of `selfRefDep` (the thing actually being replaced here) — a graft
    // needs to update that self-reference too, not just the real
    // dependency, or the result ends up pointing at its own pre-graft self
    // instead of at itself.
    let dep_old = nix_build("selfRefDep");
    let dep_new = nix_build("selfRefDepV2");
    let consumer = nix_build("selfRefConsumer");

    let run_orig = Command::new(format!("{consumer}/bin/run"))
        .output()
        .expect("failed to run original selfref-consumer");
    let orig_stdout = String::from_utf8_lossy(&run_orig.stdout).to_string();
    assert!(
        orig_stdout.contains(&format!("my own path is: {consumer}")),
        "fixture invariant: {orig_stdout}"
    );
    assert!(orig_stdout.contains("selfref dep v1"));

    let output = graft(
        &["replace", &consumer, "--override", &dep_old, &dep_new],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace on a self-referencing fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, consumer);

    let run = Command::new(format!("{grafted}/bin/run"))
        .output()
        .expect("failed to run grafted selfref-consumer");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains(&format!("my own path is: {grafted}")),
        "grafted result's self-reference should point at its own new path, not the pre-graft original: {stdout}"
    );
    assert!(
        stdout.contains("selfref dep v2"),
        "the real dependency should still be replaced too: {stdout}"
    );

    let refs = nix_store_references(&grafted);
    assert!(
        refs.contains(&grafted),
        "grafted result should genuinely self-reference its own path: {refs:?}"
    );
    assert!(
        !refs.contains(&consumer),
        "grafted result should not reference the pre-graft original: {refs:?}"
    );
    assert!(refs.contains(&dep_new));
    assert!(!refs.contains(&dep_old));
}

#[test]
fn replace_grafts_a_real_nixpkgs_package_then_ungrafts_it_back() {
    // Every other test grafts a hand-rolled fixture; this one grafts an
    // actual nixpkgs package (pigz) and its actual runtime dependency
    // (zlib), then grafts the result back to the original dependency,
    // checking the round trip reproduces the pre-graft package exactly.
    let zlib_a = nix_build("zlibA");
    let zlib_b = nix_build("zlibB");
    let pigz_a = nix_build("pigzA");
    assert!(
        nix_store_references(&pigz_a).contains(&zlib_a),
        "fixture invariant: pigzA must reference zlibA"
    );

    let compress_roundtrip = |pigz: &str| -> bool {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("data");
        fs::write(
            &input,
            b"graft integration test payload, repeated. ".repeat(1000),
        )
        .unwrap();
        let archive = dir.path().join("data.gz");
        let compress = Command::new(format!("{pigz}/bin/pigz"))
            .args(["-k", "-c"])
            .arg(&input)
            .output()
            .expect("failed to run pigz to compress");
        assert!(
            compress.status.success(),
            "pigz compress failed: {}",
            String::from_utf8_lossy(&compress.stderr)
        );
        fs::write(&archive, &compress.stdout).unwrap();
        let decompress = Command::new(format!("{pigz}/bin/pigz"))
            .args(["-d", "-c"])
            .arg(&archive)
            .output()
            .expect("failed to run pigz to decompress");
        assert!(
            decompress.status.success(),
            "pigz decompress failed: {}",
            String::from_utf8_lossy(&decompress.stderr)
        );
        decompress.stdout == fs::read(&input).unwrap()
    };
    assert!(
        compress_roundtrip(&pigz_a),
        "original pigz should round-trip compress/decompress correctly"
    );

    let grafted_output = graft(&["replace", &pigz_a, "--override", &zlib_a, &zlib_b], None);
    assert!(
        grafted_output.status.success(),
        "graft replace on a real nixpkgs package failed: {}",
        String::from_utf8_lossy(&grafted_output.stderr)
    );
    let grafted = String::from_utf8_lossy(&grafted_output.stdout)
        .trim()
        .to_string();
    assert_ne!(grafted, pigz_a);

    let grafted_refs = nix_store_references(&grafted);
    assert!(
        grafted_refs.contains(&zlib_b),
        "grafted pigz should reference the new zlib: {grafted_refs:?}"
    );
    assert!(
        !grafted_refs.contains(&zlib_a),
        "grafted pigz should not reference the old zlib: {grafted_refs:?}"
    );
    assert!(
        compress_roundtrip(&grafted),
        "grafted pigz should still round-trip compress/decompress correctly"
    );

    let ungrafted_output = graft(&["replace", &grafted, "--override", &zlib_b, &zlib_a], None);
    assert!(
        ungrafted_output.status.success(),
        "ungraft (replacing back to the original zlib) failed: {}",
        String::from_utf8_lossy(&ungrafted_output.stderr)
    );
    let ungrafted = String::from_utf8_lossy(&ungrafted_output.stdout)
        .trim()
        .to_string();

    let ungrafted_refs = nix_store_references(&ungrafted);
    assert!(
        ungrafted_refs.contains(&zlib_a),
        "ungrafted pigz should reference the original zlib again: {ungrafted_refs:?}"
    );
    assert!(
        !ungrafted_refs.contains(&zlib_b),
        "ungrafted pigz should not reference the new zlib: {ungrafted_refs:?}"
    );
    assert!(
        compress_roundtrip(&ungrafted),
        "ungrafted pigz should still round-trip compress/decompress correctly"
    );

    let dump = |p: &str| {
        Command::new("nix-store")
            .args(["--dump", p])
            .output()
            .expect("failed to run nix-store --dump")
            .stdout
    };
    assert_eq!(
        dump(&ungrafted),
        dump(&pigz_a),
        "grafting there and back should reproduce the original package's NAR exactly"
    );
}

#[test]
fn replace_warns_when_old_is_not_in_the_closure() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let unrelated_root = nix_build("chainTop");

    let output = graft(
        &["replace", &unrelated_root, "--override", &old, &new],
        None,
    );
    assert!(
        output.status.success(),
        "replace should still succeed, just warn: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        result, unrelated_root,
        "closure root should be unchanged since old was never referenced"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not in the closure of") && stderr.contains("no effect"),
        "expected the no-op warning, got: {stderr}"
    );
}

#[test]
fn replace_out_link_creates_a_gc_root_symlink() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let consumer = nix_build("consumer");

    let link_dir = tempfile::tempdir().unwrap();
    let link_path = link_dir.path().join("result");

    let output = graft(
        &[
            "replace",
            &consumer,
            "--override",
            &old,
            &new,
            "--out-link",
            link_path.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace --out-link failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let resolved =
        fs::canonicalize(&link_path).expect("--out-link should have created a resolvable symlink");
    assert_eq!(
        resolved,
        PathBuf::from(&grafted),
        "the out-link should point at the grafted result"
    );
}

#[test]
fn out_link_writes_an_appending_provenance_history() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let consumer = nix_build("consumer");

    let link_dir = tempfile::tempdir().unwrap();
    let link_path = link_dir.path().join("result");
    let history_path = link_dir.path().join("result.graft-history.jsonl");

    let args = [
        "replace",
        &consumer,
        "--override",
        &old,
        &new,
        "--out-link",
        link_path.to_str().unwrap(),
    ];

    // Run twice: history should accumulate, not just record the latest run.
    for _ in 0..2 {
        let output = graft(&args, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let history = fs::read_to_string(&history_path)
        .expect("--out-link should have written a provenance history file");
    let lines: Vec<&str> = history.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "two runs should append two lines, got: {history}"
    );

    for line in &lines {
        let entry: serde_json::Value =
            serde_json::from_str(line).expect("each history line should be valid JSON");
        assert!(
            entry["timestamp"].is_number(),
            "expected a numeric timestamp: {line}"
        );
        assert!(
            entry["command"].is_array(),
            "expected the command as an array: {line}"
        );
        assert!(
            entry["new_root"]
                .as_str()
                .is_some_and(|s| s.starts_with("/nix/store/")),
            "expected new_root: {line}"
        );
    }
}

#[test]
fn dry_run_does_not_create_an_out_link() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let consumer = nix_build("consumer");

    let link_dir = tempfile::tempdir().unwrap();
    let link_path = link_dir.path().join("result");

    let output = graft(
        &[
            "replace",
            &consumer,
            "--override",
            &old,
            &new,
            "--out-link",
            link_path.to_str().unwrap(),
            "--dry-run",
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !link_path.exists(),
        "--dry-run must not create an out-link even if --out-link is given"
    );
}

#[test]
fn replace_grafts_two_independent_nodes_at_the_same_level_in_parallel() {
    // parMidA/parMidB each need grafting (their own leaf was replaced) but
    // neither depends on the other, so they're provably independent and
    // should land in the same parallel batch.
    let a = nix_build("parLeafA");
    let a2 = nix_build("parLeafAV2");
    let b = nix_build("parLeafB");
    let b2 = nix_build("parLeafBV2");
    let top = nix_build("parallelTop");

    let output = graft(
        &[
            "-v",
            "replace",
            &top,
            "--override",
            &a,
            &a2,
            "--override",
            &b,
            &b2,
        ],
        None,
    );
    assert!(
        output.status.success(),
        "graft replace with two independent chains failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, top);

    let run = Command::new(format!("{grafted}/bin/parallel-top"))
        .output()
        .expect("failed to run grafted parallel-top");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("a v2") && stdout.contains("b v2"),
        "both chains should reach the new leaf: {stdout}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("level 1: constructing 2 recipe(s) in parallel"),
        "parMidA and parMidB should batch into the same parallel level: {stderr}"
    );
    assert!(
        stderr.contains("level 1: building 2 target(s) in one nix build call"),
        "both recipes should be built in a single batched nix build call, not two separate ones: {stderr}"
    );
    assert!(
        stderr.contains("3 grafted") && stderr.contains("2 explicit replacement"),
        "expected parMidA/parMidB/parallelTop grafted (3) plus the 2 explicit leaf targets, got: {stderr}"
    );
}

#[test]
fn replace_and_edit_combine_in_one_invocation_against_independent_nodes() {
    // --override and --edit used to be mutually exclusive subcommands; now
    // they're flags on the same command, so one branch of the closure can
    // be changed via --override while a completely independent branch is
    // changed via --edit, both grafted up through parallelTop in one pass.
    let a = nix_build("parLeafA");
    let a2 = nix_build("parLeafAV2");
    let mid_b = nix_build("parMidB");
    let top = nix_build("parallelTop");

    let mut editors = Vec::new();
    let editor = stub_editor(r#"echo "echo edited-marker" >> "$1""#, &mut editors);

    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &a,
            &a2,
            "--edit",
            &mid_b,
            "bin/mid-b",
        ],
        Some(&editor),
    );
    assert!(
        output.status.success(),
        "combined --override/--edit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grafted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_ne!(grafted, top);

    let run = Command::new(format!("{grafted}/bin/parallel-top"))
        .output()
        .expect("failed to run grafted parallel-top");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("a v2"),
        "the --override side should have propagated: {stdout}"
    );
    assert!(
        stdout.contains("edited-marker"),
        "the --edit side should have propagated too: {stdout}"
    );
    assert!(
        stdout.contains("b v1"),
        "leaf-b itself was never touched by either transform: {stdout}"
    );

    let run_orig = Command::new(format!("{top}/bin/parallel-top"))
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&run_orig.stdout).contains("edited-marker"),
        "original parallel-top must be untouched"
    );
}

#[test]
fn report_html_includes_every_changed_node_and_excludes_unchanged_ones() {
    let old = nix_build("chainLeaf");
    let new = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let report_dir = tempfile::tempdir().unwrap();
    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &old,
            &new,
            "--report",
            report_dir.path().to_str().unwrap(),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let index = report_dir.path().join("index.html");
    let html = fs::read_to_string(&index).expect("--report should have written index.html");
    assert!(html.contains("<svg"), "expected an SVG graph in the report");
    // The changed chain (leaf, mid, top) should all appear...
    for path in [&old, &mid, &top] {
        assert!(
            html.contains(path),
            "expected {path} to appear in the report"
        );
    }
    // ...but only 3 rows in the table (unchanged nodes are noise, not signal).
    assert_eq!(
        html.matches("<tr><td>").count(),
        3,
        "expected exactly 3 table rows, got:\n{html}"
    );
}

#[test]
fn report_html_shows_cutoff_nodes_but_still_excludes_unchanged_ones() {
    let old = nix_build("chainLeaf");
    let new = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let report_dir = tempfile::tempdir().unwrap();
    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &old,
            &new,
            "--cutoff",
            &mid,
            "--report",
            report_dir.path().to_str().unwrap(),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(report_dir.path().join("index.html")).unwrap();
    // The cutoff itself is a deliberate decision worth showing...
    assert!(
        html.contains("cutoff"),
        "expected the cutoff tag to appear: {html}"
    );
    assert!(
        html.contains(&mid),
        "expected the cut-off node itself to appear in the report: {html}"
    );
    // ...but `top`, left genuinely unchanged because the cutoff stopped
    // propagation before it, is still noise and must stay excluded from
    // the table (its path legitimately still appears in the page header,
    // as the unchanged closure root/new root — that's expected).
    assert_eq!(
        html.matches("<tr><td>").count(),
        2,
        "expected exactly 2 table rows (leaf, mid), got:\n{html}"
    );
    assert!(
        !html.contains(&format!("<td>{}</td>", top.rsplit('/').next().unwrap())),
        "top must not be a table row: {html}"
    );
}

/// A node's `<circle cy="...">` y-coordinate, found via the exact store
/// path inside its `<title>` tooltip — lower y is drawn higher on the page.
fn node_cy(html: &str, path: &str) -> i64 {
    let needle = format!("<title>{path}");
    let idx = html
        .find(&needle)
        .unwrap_or_else(|| panic!("no node found for {path} in report:\n{html}"));
    let before = &html[..idx];
    let cy_start = before
        .rfind("cy=\"")
        .expect("no cy=\"...\" attribute found before this node's title")
        + 4;
    let cy_end = before[cy_start..]
        .find('"')
        .expect("unterminated cy attribute")
        + cy_start;
    before[cy_start..cy_end]
        .parse()
        .expect("cy attribute should be a plain integer")
}

#[test]
fn report_html_rows_a_cutoff_node_above_the_dependency_it_left_untouched() {
    // Regression test: a cutoff's *scheduling* level is always 0 (it's
    // never built), but its *graph row* must reflect its real depth —
    // `mid` (cut off) depends on `leaf` (the explicit target one level
    // below it), so `mid` must be drawn strictly above `leaf`, not on the
    // same row as it.
    let old = nix_build("chainLeaf");
    let new = nix_build("chainLeafV2");
    let mid = nix_build("chainMid");
    let top = nix_build("chainTop");

    let report_dir = tempfile::tempdir().unwrap();
    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &old,
            &new,
            "--cutoff",
            &mid,
            "--report",
            report_dir.path().to_str().unwrap(),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(report_dir.path().join("index.html")).unwrap();
    let leaf_y = node_cy(&html, &old);
    let mid_y = node_cy(&html, &mid);
    assert!(
        mid_y < leaf_y,
        "cutoff node `mid` (cy={mid_y}) should be drawn above the dependency it left untouched, `leaf` (cy={leaf_y})"
    );
}

#[test]
fn report_html_under_dry_run_shows_pending_instead_of_a_built_path() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let consumer = nix_build("consumer");

    let report_dir = tempfile::tempdir().unwrap();
    let output = graft(
        &[
            "replace",
            &consumer,
            "--override",
            &old,
            &new,
            "--dry-run",
            "--report",
            report_dir.path().to_str().unwrap(),
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(report_dir.path().join("index.html"))
        .expect("--report should work under --dry-run too");
    assert!(
        html.contains("(dry run)"),
        "expected the dry-run marker in the report title"
    );
    assert!(
        html.contains("(pending)"),
        "expected the not-yet-built consumer to show as pending, not a real path"
    );
}

#[test]
fn report_diff_without_report_is_rejected() {
    let old = nix_build("oldDep");
    let new = nix_build("newDep");
    let consumer = nix_build("consumer");

    let output = graft(
        &[
            "replace",
            &consumer,
            "--override",
            &old,
            &new,
            "--report-diff",
        ],
        None,
    );
    assert!(
        !output.status.success(),
        "--report-diff without --report should be rejected"
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--report-diff has no effect without --report"));
}

#[test]
fn report_diff_embeds_nix_diff_for_explicit_targets_but_not_grafted_ones() {
    // `leaf` is the explicit --override target (a real derivation diff
    // exists to show); `mid`/`top` are grafted (no derivation change, so
    // no nix-diff — only diffoscope, which applies to any built old/new pair).
    let old = nix_build("chainLeaf");
    let new = nix_build("chainLeafV2");
    let top = nix_build("chainTop");

    let report_dir = tempfile::tempdir().unwrap();
    let output = graft(
        &[
            "replace",
            &top,
            "--override",
            &old,
            &new,
            "--report",
            report_dir.path().to_str().unwrap(),
            "--report-diff",
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let html = fs::read_to_string(report_dir.path().join("index.html")).unwrap();
    assert!(
        html.contains("nix-diff"),
        "expected a nix-diff block for the explicit target: {html}"
    );
    assert!(
        html.matches("diffoscope</a>").count() == 3,
        "expected a diffoscope link for all 3 changed nodes (leaf, mid, top): {html}"
    );

    // Exactly one nix-diff block (the explicit target) — mid/top (grafted)
    // must not get one, since grafting never changes the derivation.
    assert_eq!(
        html.matches("<summary>nix-diff</summary>").count(),
        1,
        "expected exactly one nix-diff block: {html}"
    );

    let diffoscope_files: Vec<_> = fs::read_dir(report_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("diffoscope-"))
        .collect();
    assert_eq!(
        diffoscope_files.len(),
        3,
        "expected 3 diffoscope HTML files on disk, found: {diffoscope_files:?}"
    );
    for f in &diffoscope_files {
        let content = fs::read_to_string(f.path()).unwrap();
        assert!(
            content.contains("<!DOCTYPE html>"),
            "expected a real diffoscope HTML report at {:?}",
            f.path()
        );
    }
}
