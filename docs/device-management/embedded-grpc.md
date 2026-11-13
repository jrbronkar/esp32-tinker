# Embedded management API audit

The following surfaces are available in the embedded migration stack. The
firmware endpoint currently runs Protolink.

| Layer | Available | Remaining work |
| --- | --- | --- |
| HTTP/2 connection | Cleartext server handshake, SETTINGS, HPACK, two active streams, frame and header limits, flow control, and stream reset handling. | The public API has no client, TLS negotiation, configurable limits, or graceful drain. The h2spec gate tracks protocol exceptions. |
| HTTP/2 stream handles | Request headers, DATA, receive credit on chunk release, request trailers, response headers, bounded writes, flush, final trailers, and peer cancellation. | The low-level frame-event and sequential helper APIs also remain public. Audit external callers before shrinking the API. A cancellation observer currently uses one waker per stream, and the server handles require a local executor and cancellation-safe socket reads. |
| gRPC runtime | Identity encoding, bounded protobuf messages, incremental bidirectional reads, flushed writes, server-streamed responses, gRPC status trailers, and reset/disconnect cancellation. | Deadline parsing and enforcement, richer status and metadata, and client-streaming with a unary response. The buffered unary and streaming entry points overlap, and generated dispatch currently exposes the HTTP/2 stream error through the gRPC crate. |
| Management service | Generated unary `Status`, server-streaming `Logs`, and bidirectional `Upload` dispatch. Host tests use Tonic and espctl. | Adapt the firmware implementation, connect the OTA flash lease to call lifetime, and verify simultaneous Logs and Upload and final ACK delivery before reboot on hardware. |

## Protobuf generation

`protocol/management.proto` is the source of RPC names, paths, request and
response types, and streaming cardinality. Cargo runs
`crates/management-grpc/build.rs` on the host, which reads protobuf descriptors
and emits the `Management` trait, `ManagementServer<S>`, and path dispatch into
`OUT_DIR`. The `no_std` crate includes that generated Rust at compile time.
Message types come from the existing micropb generator in `ota-protocol`.

The emitter is handwritten Rust and currently maps top-level `.ota` message
descriptors to `ota_protocol` types with a shared message-size limit. An
unsupported client-streaming/unary-response declaration fails the build.
Firmware and tests supply the method bodies. Reusable generation for arbitrary
proto packages would require a configurable Rust type and size-limit mapping.

## Host coverage on the Upload branch

Coverage on commit `bd126d4` used the embedded HTTP/2 and gRPC unit and
integration tests and espctl's Tonic interoperability tests.

| Source scope | Covered lines | Line coverage |
| --- | ---: | ---: |
| Embedded HTTP/2 | 1,460 / 1,510 | 96.69% |
| Embedded gRPC runtime | 340 / 362 | 93.92% |
| Both runtimes | 1,800 / 1,872 | 96.15% |
| Generated Management bindings | 22 / 26 | 84.62% |

Both runtimes had 93.09% region coverage. The gRPC streaming module had
88.83% line coverage. LLVM reported mismatched data for seven functions and
reported no branch coverage. This run does not measure the firmware endpoint
or exercise flash and reboot on hardware.
