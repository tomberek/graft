let pkgs = import <nixpkgs> {}; in rec {
  oldDep = pkgs.writeShellScriptBin "dependency" ''
    echo "got old dependency"
  '';
  newDep = pkgs.writeShellScriptBin "dependency" ''
    echo "got new dependency"
  '';
  consumer = pkgs.writeShellScriptBin "consumer" ''
    ${oldDep}/bin/dependency
  '';

  # Different-length store item names (same exposed "bin/dep" binary via
  # `destination`) — exercises --rebuild's lack of a length constraint.
  oldDepLong = pkgs.writeTextFile {
    name = "dep";
    executable = true;
    destination = "/bin/dep";
    text = ''
      #!${pkgs.bash}/bin/bash
      echo "got old long-named dependency"
    '';
  };
  newDepLong = pkgs.writeTextFile {
    name = "dependency-with-a-much-longer-name";
    executable = true;
    destination = "/bin/dep";
    text = ''
      #!${pkgs.bash}/bin/bash
      echo "got new long-named dependency"
    '';
  };
  consumerLong = pkgs.writeShellScriptBin "consumer-long" ''
    ${oldDepLong}/bin/dep
  '';

  # A 3-level chain (leaf -> mid -> top): --cutoff/--force-rebuild need a
  # middle node to apply the override to, distinct from the root or leaf.
  chainLeaf = pkgs.writeShellScriptBin "leaf" ''
    echo "leaf v1"
  '';
  chainLeafV2 = pkgs.writeShellScriptBin "leaf" ''
    echo "leaf v2"
  '';
  chainMid = pkgs.writeShellScriptBin "mid" ''
    ${chainLeaf}/bin/leaf
  '';
  chainTop = pkgs.writeShellScriptBin "top" ''
    ${chainMid}/bin/mid
  '';

  # `transitiveTop`'s .drv declares only `transitiveWrapper` as a direct
  # input, but `transitiveWrapper`'s content embeds `transitiveLeaf`'s store
  # path as text, copied verbatim into `transitiveTop`'s output — so
  # `transitiveLeaf` is a genuine runtime reference with no direct `.drv` input.
  transitiveLeaf = pkgs.writeShellScriptBin "leaf" ''
    echo "leaf v1"
  '';
  transitiveLeafV2 = pkgs.writeShellScriptBin "leaf" ''
    echo "leaf v2"
  '';
  transitiveWrapper = pkgs.writeTextFile {
    name = "wrapper";
    text = ''
      ${transitiveLeaf}/bin/leaf
    '';
  };
  transitiveTop = pkgs.runCommand "transitive-top" { } ''
    mkdir -p $out
    cp ${transitiveWrapper} $out/wrapper
  '';

  # A multi-output derivation: `out` is unrelated to the replaced
  # dependency, `extra` depends on it directly.
  multiDep = pkgs.writeShellScriptBin "multidep" ''
    echo "multidep v1"
  '';
  multiDepV2 = pkgs.writeShellScriptBin "multidep" ''
    echo "multidep v2"
  '';
  multiOut = pkgs.runCommand "multi" { outputs = [ "out" "extra" ]; } ''
    mkdir -p $out $extra
    echo "main output" > $out/data
    ln -s ${multiDep}/bin/multidep $extra/multidep
  '';
  multiConsumer = pkgs.writeShellScriptBin "multi-consumer" ''
    ${multiOut.extra}/multidep
  '';

  # A minimal stand-in for a NixOS system closure's toplevel: exposes
  # bin/switch-to-configuration (a stub that records how it was called) so
  # `graft nixos-system --switch` can be tested without a real NixOS system.
  systemLikeOldDep = pkgs.writeShellScriptBin "dependency" ''
    echo "got old dependency"
  '';
  systemLikeNewDep = pkgs.writeShellScriptBin "dependency" ''
    echo "got new dependency"
  '';
  systemLikeSwitchScript = pkgs.writeShellScript "switch-to-configuration" ''
    echo "called with: $1" > "$GRAFT_TEST_SWITCH_MARKER"
  '';
  systemLike = pkgs.runCommand "system-like" { } ''
    mkdir -p $out/bin
    ln -s ${systemLikeOldDep}/bin/dependency $out/bin/dependency
    cp ${systemLikeSwitchScript} $out/bin/switch-to-configuration
    chmod +x $out/bin/switch-to-configuration
  '';

  # A real compiled ELF binary dynamically linked against a real shared
  # library via an RPATH — every other fixture here is a shell script or
  # plain text, never actual binary content (machine code, NUL bytes, an
  # ELF .dynamic section). `binOldLib`/`binNewLib` share the same
  # name-length ("old-lib"/"new-lib", 7 chars each) so grafting succeeds;
  # the RPATH entry embedded in `binConsumer`'s ELF header is what actually
  # gets rewritten, then the dynamic linker follows it at run time.
  binOldLib = pkgs.runCommand "old-lib" { nativeBuildInputs = [ pkgs.gcc ]; } ''
    mkdir -p $out/lib
    echo 'int answer(void) { return 1; }' > lib.c
    cc -shared -fPIC -o $out/lib/libanswer.so lib.c
  '';
  binNewLib = pkgs.runCommand "new-lib" { nativeBuildInputs = [ pkgs.gcc ]; } ''
    mkdir -p $out/lib
    echo 'int answer(void) { return 2; }' > lib.c
    cc -shared -fPIC -o $out/lib/libanswer.so lib.c
  '';
  binConsumer = pkgs.runCommand "consumer-bin" { nativeBuildInputs = [ pkgs.gcc ]; } ''
    mkdir -p $out/bin
    cat > main.c <<'EOF'
    #include <stdio.h>
    int answer(void);
    int main(void) { printf("answer: %d\n", answer()); return 0; }
    EOF
    cc -o $out/bin/consumer main.c -L${binOldLib}/lib -lanswer -Wl,-rpath,${binOldLib}/lib
  '';

  # Two independent chains feeding into one root: parMidA/parMidB each need
  # grafting because their own leaf was replaced, but neither depends on
  # the other, so they belong to the same dependency level and should be
  # graftable in parallel. (The leaves themselves are the explicit
  # --override targets, resolved instantly with no build — parMidA/parMidB
  # are the first *actual* grafts, which is what makes them same-level.)
  parLeafA = pkgs.writeShellScriptBin "leaf-a" ''
    echo "a v1"
  '';
  parLeafAV2 = pkgs.writeShellScriptBin "leaf-a" ''
    echo "a v2"
  '';
  parLeafB = pkgs.writeShellScriptBin "leaf-b" ''
    echo "b v1"
  '';
  parLeafBV2 = pkgs.writeShellScriptBin "leaf-b" ''
    echo "b v2"
  '';
  parMidA = pkgs.writeShellScriptBin "mid-a" ''
    ${parLeafA}/bin/leaf-a
  '';
  parMidB = pkgs.writeShellScriptBin "mid-b" ''
    ${parLeafB}/bin/leaf-b
  '';
  parallelTop = pkgs.writeShellScriptBin "parallel-top" ''
    ${parMidA}/bin/mid-a
    ${parMidB}/bin/mid-b
  '';

  # A real nixpkgs package (pigz) with a real runtime dependency (zlib) —
  # every other fixture here is hand-rolled (writeShellScriptBin, a two-line
  # runCommand). `zlibB` is a cosmetic rebuild of the exact same zlib
  # source with only `ZLIB_VERSION` patched (a version-banner macro, not an
  # ABI-affecting one — zlib's SONAME, exported symbols, and `libz.so.1`
  # symlink are untouched), so the graft is safe but the two outputs are
  # genuinely distinguishable at runtime via `zlibVersion()`.
  zlibA = pkgs.zlib;
  zlibB = pkgs.zlib.overrideAttrs (old: {
    postPatch = (old.postPatch or "") + ''
      sed -i 's/#define ZLIB_VERSION "[^"]*"/#define ZLIB_VERSION "1.3.2-grafted"/' zlib.h
    '';
  });
  pigzA = pkgs.pigz.override { zlib = zlibA; };

  # Embeds its own `$out` in its content (a genuine self-reference,
  # distinct from `selfRefDep`) *and* depends on something that changes —
  # exercises graft's self-reference rewrite, not just its normal
  # dependency-substitution path.
  selfRefDep = pkgs.writeShellScriptBin "selfref-dep" ''echo "selfref dep v1"'';
  selfRefDepV2 = pkgs.writeShellScriptBin "selfref-dep" ''echo "selfref dep v2"'';
  selfRefConsumer = pkgs.runCommand "selfref-consumer" { } ''
    mkdir -p $out/bin
    cat > $out/bin/run <<EOF
    #!${pkgs.bash}/bin/bash
    echo "my own path is: $out"
    ${selfRefDep}/bin/selfref-dep
    EOF
    chmod +x $out/bin/run
  '';

  # Two versions of a multi-output derivation, both outputs genuinely
  # referenced (via symlinks, so Nix's reference scanner picks them up) by
  # multiAllConsumer — exercises `--override old^* new^*` pairing every
  # output by name in one shot, instead of one `--override` per output.
  multiAllOld = pkgs.runCommand "multi-all" { outputs = [ "out" "extra" ]; } ''
    mkdir -p $out $extra
    echo "out v1" > $out/data
    echo "extra v1" > $extra/data
  '';
  multiAllNew = pkgs.runCommand "multi-all" { outputs = [ "out" "extra" ]; } ''
    mkdir -p $out $extra
    echo "out v2" > $out/data
    echo "extra v2" > $extra/data
  '';
  multiAllConsumer = pkgs.runCommand "multi-all-consumer" { } ''
    mkdir -p $out
    ln -s ${multiAllOld} $out/out-link
    ln -s ${multiAllOld.extra} $out/extra-link
  '';

  # Starts with zero references; --edit-ing config to embed one should
  # register it, exercising store::scan_references + the synthetic-
  # derivation re-add (nix store add alone never registers anything).
  editRefDep = pkgs.writeShellScriptBin "edit-ref-dep" ''echo "edit ref dep"'';
  editRefTarget = pkgs.runCommand "edit-ref-target" { } ''
    mkdir -p $out
    echo "nothing here yet" > $out/config
  '';

  # Embeds just the 32-character hash of bareHashDep, with no "-name"
  # suffix following — Nix's own post-build scan matches the bare hash
  # alone (confirmed via nix-store -q --references on this fixture
  # itself), narrower than our own sed rule used to.
  bareHashDep = pkgs.writeShellScriptBin "bare-dep" ''echo "bare dep v1"'';
  bareHashDepV2 = pkgs.writeShellScriptBin "bare-dep" ''echo "bare dep v2"'';
  bareHashConsumer = pkgs.runCommand "bare-hash-consumer" { } ''
    mkdir -p $out
    echo "${builtins.substring 0 32 (baseNameOf "${bareHashDep}")}" > $out/bare-ref
  '';
}
