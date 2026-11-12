# Repository guidance

- Follow [CONTRIBUTING.md](CONTRIBUTING.md) for the pinned Nix toolchain,
  host checks, and ESP32-S3 checks. Report the checks actually run in the PR.
- Keep changes focused and reuse existing code when it preserves behavior.
  Keep portable runtime crates compatible with their existing `no_std`
  targets. Check transport and firmware changes on the ESP32-S3 target.
- Propagate failures with typed errors. Avoid introducing `unwrap`, `expect`,
  or explicit panics in Rust changes. In tests, use assertions for
  expectations and propagate setup and I/O errors with `?`.
- Give protocol flags, byte offsets, status codes, and buffer limits
  descriptive names or use the existing protocol crate's definitions. Link
  to the relevant specification when a wire rule is easy to misread. Keep
  literal fixture bytes when they independently verify wire encoding, and
  explain what they represent.
- For new protocol code, aim for near-complete coverage of success and error
  paths. Exercise public entry points with interoperable peers when possible.
  Report measured coverage, including the metric, scope, and test suites.
  Describe significant gaps and distinguish host checks from board tests.
- Document implementation choices such as buffering, backpressure, and
  lifecycle behavior next to the relevant design or protocol documentation.
