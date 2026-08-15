{
  description = "Rust development environment for ESP32-S3 firmware";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    nixpkgs,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachSystem ["aarch64-darwin"] (system: let
      pkgs = import nixpkgs {inherit system;};

      espRustVersion = "1.95.0.0";
      espRust = pkgs.stdenvNoCC.mkDerivation {
        pname = "esp-rust";
        version = espRustVersion;

        src = pkgs.fetchurl {
          url = "https://github.com/esp-rs/rust-build/releases/download/v${espRustVersion}/rust-${espRustVersion}-aarch64-apple-darwin.tar.xz";
          hash = "sha256-VDrdluRSzFmNOdHf3w8q/2z9xUrlbOLmOtFXxneLXSs=";
        };

        rustSrc = pkgs.fetchurl {
          url = "https://github.com/esp-rs/rust-build/releases/download/v${espRustVersion}/rust-src-${espRustVersion}.tar.xz";
          hash = "sha256-cIvuM3rC1BwOhhr5MEe//skaBS+8tJV5JdhApB8ydxc=";
        };

        nativeBuildInputs = [pkgs.xz];
        dontUnpack = true;

        installPhase = ''
          runHook preInstall

          tar -xJf "$src" \
            rust-nightly-aarch64-apple-darwin/install.sh \
            rust-nightly-aarch64-apple-darwin/components \
            rust-nightly-aarch64-apple-darwin/rust-installer-version \
            rust-nightly-aarch64-apple-darwin/cargo \
            rust-nightly-aarch64-apple-darwin/rustc \
            rust-nightly-aarch64-apple-darwin/rust-std-aarch64-apple-darwin

          cd rust-nightly-aarch64-apple-darwin
          ./install.sh \
            --prefix="$out" \
            --disable-ldconfig \
            --components=cargo,rustc,rust-std-aarch64-apple-darwin

          mkdir -p "$out/lib/rustlib/src"
          tar -xJf "$rustSrc" --strip-components=5 \
            -C "$out/lib/rustlib/src" \
            rust-src-nightly/rust-src/lib/rustlib/src

          runHook postInstall
        '';
      };

      xtensaGccVersion = "15.2.0_20250920";
      xtensaGcc = pkgs.stdenvNoCC.mkDerivation {
        pname = "xtensa-esp-elf";
        version = xtensaGccVersion;

        src = pkgs.fetchurl {
          url = "https://github.com/espressif/crosstool-NG/releases/download/esp-${xtensaGccVersion}/xtensa-esp-elf-${xtensaGccVersion}-aarch64-apple-darwin.tar.xz";
          hash = "sha256-b0i3kcW4STOaUhoADMvGzYPnAq02x91pg9BX0MhovjM=";
        };

        installPhase = ''
          runHook preInstall
          mkdir -p "$out"
          cp -R . "$out"
          runHook postInstall
        '';
      };
    in {
      formatter = pkgs.alejandra;

      packages = {
        esp-rust = espRust;
        xtensa-gcc = xtensaGcc;
      };

      devShells.default = pkgs.mkShell {
        packages = [
          espRust
          xtensaGcc
          pkgs.espflash
          pkgs.pkg-config
        ];
      };
    });
}
