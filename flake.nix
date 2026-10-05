{
  description = "graft: Guix-style graft/rewrite prototype for the Nix store";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      forAllSystems = f: nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ] f;
    in
    {
      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system}; in
        {
          default = pkgs.mkShell {
            # bash/gnused must be store paths (declarable as derivation
            # inputs); the ambient /usr/bin/bash isn't.
            # nix-diff/diffoscope/binutils (for readelf) are diagnostic
            # tooling, not core functionality — dev-shell only, not in the
            # packaged binary's wrapped PATH.
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.nix
              pkgs.bash
              pkgs.gnused
              pkgs.nix-diff
              pkgs.diffoscope
              pkgs.binutils
            ];
          };
        });

      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system}; in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "graft";
            version = "0.1.0";
            src = self;
            cargoLock = { lockFile = ./Cargo.lock; };
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram $out/bin/graft \
                --prefix PATH : ${pkgs.nix}/bin:${pkgs.bash}/bin:${pkgs.gnused}/bin
            '';
            # Tests need a live Nix daemon, which the build sandbox denies.
            # Run via `nix develop -c cargo test` instead.
            doCheck = false;
          };
        });
    };
}
