{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    crane.url = "github:ipetkov/crane";
    systems.url = "github:nix-systems/default";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils = {
      url = "github:numtide/flake-utils";
      inputs.systems.follows = "systems";
    };
  };

  outputs = {
    nixpkgs,
    crane,
    rust-overlay,
    flake-utils,
    ...
  }: let
    rustToolchain = p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    metorOverlay = final: prev: {
      metor = {
        memserve = final.callPackage ./nix/pkgs/memserve.nix {inherit crane rustToolchain;};
        metor-cli = final.callPackage ./nix/pkgs/metor-cli.nix {inherit crane rustToolchain;};
        metor-py = final.callPackage ./nix/pkgs/metor-py.nix {inherit crane rustToolchain;};
        metor-db = final.callPackage ./images/aleph/pkgs/metor-db.nix {inherit crane rustToolchain;};
      };
    };
  in
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = (nixpkgs.legacyPackages.${system}.extend rust-overlay.overlays.default).extend metorOverlay;
        config.packages = pkgs.metor;
        docs-image = pkgs.callPackage ./nix/docs.nix {inherit config;};
        devShells = pkgs.callPackage ./nix/shell.nix {inherit config rustToolchain;};
      in {
        packages = with pkgs.metor;
          {
            inherit memserve metor-db metor-cli metor-py;
          }
          // pkgs.lib.attrsets.optionalAttrs pkgs.stdenv.isLinux {
            inherit docs-image;
          };
        devShells = with devShells;
          {
            inherit c ops python nix-tools writing docs;
          }
          // pkgs.lib.attrsets.optionalAttrs pkgs.stdenv.isLinux {
            inherit rust;
          };
      }
    );
}
