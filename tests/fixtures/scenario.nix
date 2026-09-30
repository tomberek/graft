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
  # --replace targets, resolved instantly with no build — parMidA/parMidB
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
}
