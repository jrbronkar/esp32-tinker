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

Run from the repository root. The Nix command supplies the pinned compiler
and matching LLVM tools, runs the host tests, and prints a coverage table
and the path to an HTML report.

```sh
nix run .#coverage
```

To measure a specific component, repeat `--source` for each source path and
pass Cargo test selectors after the second `--`. For example, combine the
HTTP/2 and gRPC interoperability tests while reporting the HTTP/2 sources.

```sh
nix run .#coverage -- --source crates/embedded-http2/src -- \
  -p embedded-http2 -p espctl --test interop --test grpc_interop
```

Each run creates fresh profiles under `target/coverage-build/report.*` and
reuses the instrumented build. Set `COVERAGE_BUILD_DIR` to use another
directory. Host tests may require loopback access. In a PR, report the test
suites, source scope, and metric, such as covered lines over total lines.
List ESP32-S3 and board checks separately.

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
