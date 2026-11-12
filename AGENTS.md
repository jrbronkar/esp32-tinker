# Repository guidance

- Follow [CONTRIBUTING.md](CONTRIBUTING.md) for the toolchain and verification
  commands. Run the checks relevant to the change and report the results.
- Keep changes focused, reuse existing code where it fits, and preserve the
  behavior of supported callers and targets.
- Propagate recoverable failures with typed errors. Avoid introducing
  `unwrap`, `expect`, or explicit panics in Rust changes. In tests, assert
  expected behavior and propagate setup and I/O errors with `?`.
- Name values with domain meaning and reuse existing constants. Link to
  standards or design references when they explain a requirement. Literal
  fixtures are useful when they independently verify a format or wire encoding.
- Test new behavior and important failure paths through normal entry points.
  Aim for near-complete coverage of new code. When reporting coverage,
  identify the metric, scope, and tests run, and call out meaningful gaps.
- Document behavior and design decisions where future maintainers will look
  for them. Keep examples and related documentation in sync with the code.
