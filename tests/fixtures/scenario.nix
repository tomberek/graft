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
}
