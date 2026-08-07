{
  description = "Static musl build environment for walkie-relay (runs in a scratch container)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        rust = pkgs.rust-bin.stable."1.97.1".default.override {
          targets = ["x86_64-unknown-linux-musl"];
        };
        muslCC = pkgs.pkgsCross.musl64.stdenv.cc;
        ccBin = "${muslCC}/bin/${muslCC.targetPrefix}cc";
      in {
        devShells.default = pkgs.mkShell {
          packages = [rust muslCC];

          # `ring` compiles C for the musl target; point the cc crate and the
          # cargo linker at the musl cross toolchain. Rust's musl target links
          # the CRT statically by default, so the resulting binary has no glibc
          # dependency and runs in a `scratch` image — exactly what the tiny
          # wondering.xyz VPS needs (it can't compile this tree itself).
          CC_x86_64_unknown_linux_musl = ccBin;
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = ccBin;
        };
      }
    );
}
