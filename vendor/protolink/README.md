# Vendored Protolink

These four crates come from upstream commit
`62cc2c87a687ba65fc6885e7c9691cefacf2d064`. The GitHub repository and
crates.io packages became unavailable, so we keep the pinned source here
to restore reproducible local and CI builds. The reason for the removal
is unknown. We can evaluate a replacement separately.

## Upstream references

These URLs record the original dependency and package locations. They
were unavailable when this copy was vendored.

- [GitHub repository](https://github.com/thives/protolink.rs)
- [Pinned GitHub commit](https://github.com/thives/protolink.rs/commit/62cc2c87a687ba65fc6885e7c9691cefacf2d064)
- [protolink](https://crates.io/crates/protolink)
- [protolink-grpc 0.0.5](https://crates.io/crates/protolink-grpc/0.0.5)
- [protolink-grpc-gen 0.0.4](https://crates.io/crates/protolink-grpc-gen/0.0.4)
- [protolink-http2 0.0.4](https://crates.io/crates/protolink-http2/0.0.4)

## License and local changes

Each vendored crate declares `MIT OR Apache-2.0` at the pinned revision.
Those terms permit redistribution. This copy retains the upstream
[MIT license](LICENSE-MIT), [Apache 2.0 license](LICENSE-APACHE), and
copyright notices in the included files.

We include the library and code generator sources used by this workspace.
Upstream integration tests, test-only dependencies, and unrelated crates
are omitted. Local manifest edits remove the omitted test dependencies
and redirect two dependencies to the local copies described below. The
Rust sources are unchanged.

## ZeroDDS attribution

[`protolink-http2/src/huffman.rs`](protolink-http2/src/huffman.rs)
retains the upstream Apache-2.0 SPDX header and the ZeroDDS Contributors
copyright notice. Its 257 Huffman table entries match the published
[`zerodds-hpack 1.0.0-rc.6`](https://crates.io/crates/zerodds-hpack/1.0.0-rc.6)
source. Apply the [Apache 2.0 license](LICENSE-APACHE) to this file even
when using the MIT option for the rest of the dual-licensed code.
The published ZeroDDS crate archive contains no separate NOTICE file.
Include the Apache license and attribution when distributing firmware
that contains this code.

## Public removal records

The [crates.io index](https://github.com/rust-lang/crates.io-index/commit/3b9d773372f813479eddf7145c775cc7f353f603)
records the deletion without a reason. A November 11, 2026 search of the
[RustSec advisory database](https://github.com/RustSec/advisory-db) found
no entries mentioning the six crate names or the upstream account
`thives`. A search of [GitHub's public DMCA notices](https://github.com/github/dmca)
found no matching notice. These searches leave the reason for removal
unknown.

## COBS and ARQ dependencies

Protolink's async feature resolves `cobs-io-async` and `arq-io-async`.
Their crates.io index entries were deleted on November 10, 2026. We
vendored the exact published archives from the local Cargo cache after
matching their SHA-256 checksums to the previous `Cargo.lock`. Both
declare `MIT OR Apache-2.0`. Each directory retains its upstream license
files and source notices. The imported Rust files are unchanged. Local
manifest edits point Protolink to these copies and omit upstream
benchmark targets and dev-dependencies.

| Crate | Original GitHub repository | Original crates.io release | Index deletion |
| --- | --- | --- | --- |
| `cobs-io-async 0.0.4` | [thives/cobs-io-async.rs](https://github.com/thives/cobs-io-async.rs) | [0.0.4](https://crates.io/crates/cobs-io-async/0.0.4) | [November 10](https://github.com/rust-lang/crates.io-index/commit/4f9156d7a5e3e5bdd356f5adb8b8ddd7c152bc03) |
| `arq-io-async 0.0.3` | [thives/arq-io-async.rs](https://github.com/thives/arq-io-async.rs) | [0.0.3](https://crates.io/crates/arq-io-async/0.0.3) | [November 10](https://github.com/rust-lang/crates.io-index/commit/d1067e3babcf416c740968c1c4c48409400e61d6) |
