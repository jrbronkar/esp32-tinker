# Native gRPC feasibility

Status: **out of scope**, decided 2026-11-03. Historical research only.
Neither gRPC server/client work nor a gateway or transport migration is planned
for this implementation or PR. Keep the current Protobuf-over-TCP transport.

The investigation found a candidate that cross-compiles with our Embassy socket
and timer. It did not establish linked firmware, memory-budget, interoperability,
or hardware results. The hypothetical steps below are not an active roadmap.

## Candidate assessment

| Candidate | Fit | Decision |
| --- | --- | --- |
| Tonic's standard server | Uses Hyper/Tokio transport; not a drop-in for our `no_std` Embassy firmware | Suitable host reference client, not chosen for device runtime |
| Protolink 0.0.5 | `no_std + alloc`; async embedded-io 0.7; HTTP/2 and native gRPC; documented unary and streaming support | Best candidate found for a constrained device spike |
| Building a gRPC codec over a separate HTTP/2 library | Possible but makes us own more interoperability and lifecycle code | Avoid unless the integrated candidate fails |

Sources: [Tonic server](https://docs.rs/tonic/0.14.6/tonic/transport/server/struct.Server.html),
[Protolink repository](https://github.com/thives/protolink.rs),
[Protolink 0.0.5 API](https://docs.rs/protolink/0.0.5/protolink/),
[gRPC core API](https://docs.rs/protolink-grpc/0.0.5/protolink_grpc/).

Protolink 0.0.5 was published 2026-10-26 and declares Rust 1.88 as its minimum;
our compiler is 1.95. This is a very recent dependency, not an established
production choice. Verify published source and lock transitive versions; do not
rely on repository HEAD matching the release.

## Compile experiment

An isolated scratch library used these dependencies (no firmware workspace or
lockfile changes):

```toml
[dependencies]
protolink = { version = "=0.0.5", default-features = false, features = ["async"] }
embassy-net = { version = "=0.9.1", features = ["medium-ethernet", "tcp", "proto-ipv4"] }
embassy-time = "0.5.1"
```

Its `#![no_std]` library implemented `protolink::Clock` using
`embassy_time::Instant` and `protolink::Timer::sleep_until` using
`embassy_time::Timer::at`. The server adapter type-checked:

```rust
pub async fn serve<H: protolink::Handler>(
    socket: embassy_net::tcp::TcpSocket<'_>,
    handler: &mut H,
) {
    let _ = protolink::server(socket)
        .with_deadline_timer(EmbassyClock)
        .with_max_message_size(4608)
        .build()
        .serve(handler)
        .await;
}
```

Checked with the pinned compiler, target `xtensa-esp32s3-none-elf`, and
`-Zbuild-std=core,alloc`. Resolved Protolink HTTP/2 0.0.4 and zerodds HTTP/2/HPACK
1.0.0-rc.7. The first attempt omitted `proto-ipv4`, causing smoltcp configuration
errors; adding the same IPv4 capability used by our network fixed it.

The probe verifies dependency and adapter compatibility, not generated service
bindings or a concrete handler. It intentionally discards the server result;
real firmware must log failures and release/accept connections appropriately.
No TLS feature or codec backend was enabled. No code was flashed.

## Memory and integration constraints

- Our firmware declares 64 KiB + 36 KiB heaps, also used by the radio and other
  allocations. Free memory after networking is currently unmeasured.
- Published `protolink-http2` defaults are eight concurrent streams and a 65,535-
  byte initial stream window. The gRPC server default sets the connection window
  to their product: 524,280 bytes. This is an allowed buffering budget, not a
  measurement of immediate allocation. It is unsuitable as our device default.
- A 4,608-byte message limit alone does not bound aggregate header, stream,
  pending-output, and connection buffering. Explicitly constrain those separately.
- Start with one connection/active RPC, no compression, constrained headers, and
  measured buffers. Set stream windows large enough to make progress with the
  chosen message decoding scheme; do not guess that a sub-message window works.
- The current socket/driver versions use embedded-io-async 0.7, so the direct
  transport trait compatibility check passes. Cancellation safety and behavior
  under partial reads still need actual interop tests.
- Protolink's optional micropb backend depends on micropb 0.6; our generated types
  use micropb 0.7. These are different Rust traits. Do not silently downgrade the
  existing schema generator or assume their generated bindings are interchangeable.
  For the spike, implement the raw Handler interface and use our own 0.7 codec;
  evaluate generated service glue or an upstream compatibility update afterward.
- Standard gRPC error trailers, deadlines and stream cancellation do not provide
  update authorization, power-failure safety, or application health confirmation.

See [HTTP/2 API](https://docs.rs/protolink-http2/0.0.4/protolink_http2/) and
[server configuration](https://docs.rs/protolink-grpc/0.0.5/protolink_grpc/struct.ServerConfig.html).

## Historical evaluation outline (not scheduled)

1. **Read-only status spike.** Add an opt-in experimental gRPC endpoint on a
   separate port, retaining the proven TCP service. Add a service/method to a
   dedicated schema; reuse status data and existing micropb messages. Use plain
   HTTP/2 prior knowledge for this controlled development experiment. Limit
   connections/streams/headers explicitly and enforce deadlines.
2. **Interoperability.** Query from an independent standard gRPC client (Tonic or
   grpcurl with the proto supplied explicitly). Reflection is not required.
   Test fragmented traffic, unknown paths, bad messages, cancellations, deadlines,
   idle peers, oversized headers and flow-control stalls. Never call merely
   exchanging a five-byte gRPC message prefix over TCP a native gRPC result.
3. **Link and measure on ESP32.** Record image size, static/task memory, free and
   peak heap after Wi-Fi, connection, status RPC and repeated connect/disconnect.
   Compare latency with the current service. Test exhausted-memory behavior and
   listener recovery. Compile success alone is not this gate.
4. **Update lifecycle design.** Prefer one bidirectional update RPC carrying
   Begin/Chunk/Finish/Commit/Abort requests and acknowledgements. This preserves
   connection-local transfer ownership while using standard HTTP/2/gRPC framing.
   Unary operations would instead need explicit update IDs, expiry and ownership
   across calls; do not reuse the current state machine without designing that.
   Keep chunk and message limits, stored-image verification and explicit commit.
5. **Streaming upload gates.** Exercise an image larger than HTTP/2 windows,
   slow/non-reading clients, cancellation after writes, disconnect after verified,
   and commit reply loss. Keep exactly one writer even if HTTP/2 allows concurrent
   streams. Verify that blocking flash operations do not starve the network.
6. **Migration.** Only replace the current transport after status, memory and
   streaming gates pass on hardware and the regression/fault suite is ported.
   Keep authentication and reliable rollback as explicit requirements.

The earlier status-spike proposal is superseded by the explicit out-of-scope
decision. No gRPC implementation work is scheduled.
