{
  description = "Rust development environment for ESP32-S3 firmware";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {nixpkgs, flake-utils, ...}:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {inherit system;};
    in {
      formatter = pkgs.alejandra;

      devShells.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          espflash
          pkg-config
          rustc
          rustup
        ];

        shellHook = ''
          if [ -f "$HOME/export-esp.sh" ]; then
            . "$HOME/export-esp.sh"
          elif ! command -v espup >/dev/null 2>&1; then
            echo "ESP Xtensa toolchain not installed for this repository."
            echo "Run: cargo install espup --locked"
            echo "Then: espup install --targets esp32s3"
          fi
        '';
      };
    });
}
