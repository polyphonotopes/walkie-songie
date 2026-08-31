{
  description = "walkie-songie Tauri/Iroh development environment";

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
          extensions = ["clippy" "rustfmt"];
          targets = ["wasm32-unknown-unknown"];
        };
      in {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rust
            chromium
            trunk
            nodejs_24
            pnpm
            pkg-config
            wrapGAppsHook3
            llvmPackages.llvm
          ];

          # `ring` (iroh's tls-ring, pulled in by the wasm `browser-net`
          # feature) compiles C for wasm32-unknown-unknown. The nix-wrapped
          # clang injects native glibc flags that break wasm objects, so point
          # the cc crate at the UNWRAPPED clang + llvm-ar explicitly.
          CC_wasm32_unknown_unknown = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
          AR_wasm32_unknown_unknown = "${pkgs.llvmPackages.llvm}/bin/llvm-ar";

          buildInputs = with pkgs; [
            alsa-lib
            dbus
            glib
            gtk3
            jack2
            libayatana-appindicator
            librsvg
            libsoup_3
            openssl
            udev
            webkitgtk_4_1
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.alsa-lib
            pkgs.jack2
            pkgs.libayatana-appindicator
            pkgs.udev
          ];
        };
      }
    );
}
