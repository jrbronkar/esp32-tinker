# Contributing

Run commands from the repository root. The Nix flake supports Apple Silicon
macOS and provides the pinned ESP Rust compiler, Xtensa linker, rustfmt, Clippy,
and espflash.

```sh
nix develop
cargo clippy --version
cargo fmt --version
```

## Host checks

The [CI workflow](.github/workflows/ci.yml) runs the host and firmware checks
on pull requests and pushes to `master`.

Plain Cargo commands select `espctl` and the portable crates in the root
workspace. Run these before submitting a PR:

```sh
cargo build --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

Use `cargo fmt --all` to apply formatting. Host tests need neither a board nor
Wi-Fi credentials. The standalone visualizers have their own Cargo workspaces
and can be built with `nix build .#imu-visualizer .#soil-visualizer`.

## Host coverage reports

Use the host toolchain from the development shell. Put `llvm-profdata`,
`llvm-cov`, and `jq` on `PATH`. Use the LLVM tools for the major version
reported by `rustc -vV`. The pinned ESP Rust toolchain uses LLVM 21.

The example combines the HTTP/2 and gRPC interoperability tests and reports
coverage for the `embedded-http2` sources. Run it from the repository root
in Bash. The tests use local loopback sockets.

```bash
(
  set -eo pipefail
  export CARGO_TARGET_DIR="${COVERAGE_BUILD_DIR:-$PWD/target/coverage-build}"
  mkdir -p "$CARGO_TARGET_DIR"
  coverage_run="$(mktemp -d "$CARGO_TARGET_DIR/report.XXXXXX")"
  printf 'Coverage run at %s\n' "$coverage_run"
  export LLVM_PROFILE_FILE="$coverage_run/%p-%m.profraw"
  export RUSTFLAGS="-C instrument-coverage"
  export CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_TEST_OPT_LEVEL=0

  cargo test --locked -p embedded-http2 -p espctl \
    --test interop --test grpc_interop --message-format=json \
    > "$coverage_run/cargo.json"

  objects=()
  while IFS= read -r binary; do
    objects+=(-object "$binary")
  done < <(jq -Rr '
    fromjson?
    | select(.reason == "compiler-artifact" and .profile.test and .executable)
    | .executable
  ' "$coverage_run/cargo.json")

  llvm-profdata merge -sparse "$coverage_run"/*.profraw \
    -o "$coverage_run/coverage.profdata"
  llvm-cov report "${objects[@]}" -instr-profile="$coverage_run/coverage.profdata" \
    --sources crates/embedded-http2/src/*.rs
  llvm-cov show "${objects[@]}" -instr-profile="$coverage_run/coverage.profdata" \
    -format=html -output-dir="$coverage_run/html" --sources crates/embedded-http2/src/*.rs
  printf 'HTML report at %s/html/index.html\n' "$coverage_run"
)
```

Change the Cargo package and test selectors and the source paths to measure
another part of the workspace. Each run uses fresh profile files and reuses
the instrumented build directory. Set `COVERAGE_BUILD_DIR` to use a different
location. In a PR, include the test suites, source
scope, and metric, such as covered lines over total lines. These are host
measurements. List ESP32-S3 and board checks separately.

## ESP32-S3 checks

Select the firmware package and target explicitly. For a build and lint check
of `device-demo`, use placeholders for its required Wi-Fi settings:

```sh
WIFI_SSID=build-check WIFI_PASSWORD=build-check cargo build --locked \
  --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc --release -p device-demo

WIFI_SSID=build-check WIFI_PASSWORD=build-check cargo clippy --locked \
  --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc -p device-demo -- -D warnings
```

For hardware tests, rebuild with your local Wi-Fi settings. Firmware embeds
those settings, so keep credentials and built images out of Git. See the
[provisioning guide](docs/ota/provisioning.md) and [OTA probes](docs/ota/testing.md).

In the PR, describe the behavior change and list the checks you ran. Include
board results when the change affects hardware behavior.
