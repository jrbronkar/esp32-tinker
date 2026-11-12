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
are omitted. The only edits to the imported files remove dev-dependencies
from the manifests for the omitted tests. The Rust sources are unchanged.
