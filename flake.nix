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
            rust-nightly-aarch64-apple-darwin/clippy-preview \
            rust-nightly-aarch64-apple-darwin/rustfmt-preview \
            rust-nightly-aarch64-apple-darwin/rustc \
            rust-nightly-aarch64-apple-darwin/rust-std-aarch64-apple-darwin

          cd rust-nightly-aarch64-apple-darwin
          ./install.sh \
            --prefix="$out" \
            --disable-ldconfig \
            --components=cargo,clippy-preview,rustfmt-preview,rustc,rust-std-aarch64-apple-darwin

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

      firmwareTools = [
        espRust
        xtensaGcc
        pkgs.espflash
        pkgs.pkg-config
      ];

      otaImage = pkgs.writeShellApplication {
        name = "ota-image";
        runtimeInputs = firmwareTools ++ [pkgs.coreutils];
        text = ''
          if [[ $# -ne 1 || ! $1 =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]{0,30}$ ]]; then
            echo "Usage: nix run .#ota-image -- VERSION (1–31 letters, digits, dots, underscores or hyphens)" >&2
            exit 2
          fi
          if [[ ! -f flake.nix || ! -d crates/device-demo ]]; then
            echo "Run from the ESP32 repository root." >&2
            exit 2
          fi
          : "''${WIFI_SSID:?Export WIFI_SSID before building}"
          : "''${WIFI_PASSWORD?Export WIFI_PASSWORD before building}"
          version=$1
          export FIRMWARE_VERSION="$version"
          cargo build --locked --target-dir target --target xtensa-esp32s3-none-elf \
            -Zbuild-std=core,alloc --release -p device-demo
          mkdir -p target/ota
          temp=$(mktemp target/ota/.image.XXXXXXXX)
          trap 'rm -f "$temp"' EXIT
          espflash save-image --chip esp32s3 --flash-size 16mb \
            target/xtensa-esp32s3-none-elf/release/device-demo "$temp"
          output="target/ota/$version.bin"
          mv -f "$temp" "$output"
          printf 'OTA image: %s\n' "$output"
        '';
      };

      espctl = pkgs.writeShellApplication {
        name = "espctl";
        runtimeInputs = [espRust pkgs.pkg-config];
        text = ''
          exec cargo run --locked --quiet -p espctl -- "$@"
        '';
      };

      soilVisualizer = pkgs.rustPlatform.buildRustPackage {
        pname = "soil-visualizer";
        version = "0.1.0";
        src = ./tools/soil-visualizer;
        cargoLock.lockFile = ./tools/soil-visualizer/Cargo.lock;
      };

      imuVisualizer = pkgs.rustPlatform.buildRustPackage {
        pname = "imu-visualizer";
        version = "0.1.0";
        src = ./tools/imu-visualizer;
        cargoLock.lockFile = ./tools/imu-visualizer/Cargo.lock;
      };
    in {
      formatter = pkgs.alejandra;

      packages = {
        esp-rust = espRust;
        imu-visualizer = imuVisualizer;
        soil-visualizer = soilVisualizer;
        xtensa-gcc = xtensaGcc;
      };

      apps = {
        espctl = flake-utils.lib.mkApp {drv = espctl;};
        ota-image = flake-utils.lib.mkApp {drv = otaImage;};
        imu-visualizer = flake-utils.lib.mkApp {drv = imuVisualizer;};
        soil-visualizer = flake-utils.lib.mkApp {drv = soilVisualizer;};
      };

      devShells.default = pkgs.mkShell {
        packages = firmwareTools;

        shellHook = ''
          export PS1='\n\[\033[1;34m\][esp32:\w:⚡]\$ \[\033[0m\]'
        '';
      };
    });
}
