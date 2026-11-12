# Repository guidance

- Follow [CONTRIBUTING.md](CONTRIBUTING.md) for build, test, and coverage commands.
  Run the checks relevant to the change and report what passed and what remains.
- Keep changes focused. Prefer simple control flow and reuse existing APIs
  when they preserve behavior for supported callers and targets.
- Parse external input at the boundary into domain types that carry the
  invariants callers rely on. Return typed errors for invalid input and check
  rules that depend on current state where the operation runs.
- Propagate recoverable failures with typed errors and leave state consistent
  after errors, cancellation, or retries. Avoid introducing `unwrap`, `expect`,
  or explicit panics in Rust changes.
- Keep resource use proportional to the work. Bound buffers and queues, avoid
  unnecessary allocation, copying, and repeated work, and measure performance
  when a change is intended to improve it.
- Use descriptive names for meaningful values. Document the constraints and
  tradeoffs behind decisions that are hard to infer from the code.
- Test observable behavior and important failure paths through normal entry
  points. Aim for near-complete coverage of changed behavior and report the
  measured scope and significant gaps.
