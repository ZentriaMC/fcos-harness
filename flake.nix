{
  description = "fcos-harness";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, flake-utils, rust-overlay, ... }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
    in
    flake-utils.lib.eachSystem supportedSystems (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
            pkgs.apple-sdk_15
          ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        fcos-harness = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });
      in
      {
        packages = {
          default = fcos-harness;
          inherit fcos-harness;
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.butane
            pkgs.qemu
          ];

          BUTANE = "${pkgs.butane}/bin/butane";
          QEMU_EFI_FW =
            if pkgs.stdenv.isx86_64 then "${pkgs.qemu}/share/qemu/edk2-x86_64-code.fd"
            else if pkgs.stdenv.isAarch64 then "${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd"
            else "UNSUPPORTED";
        };
      });
}
