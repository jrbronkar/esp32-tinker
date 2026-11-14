# Embedded management API audit

The firmware serves `esp.experimental.Management` over cleartext HTTP/2
using the generated `ManagementServer` and
`embedded_grpc::serve_streaming_with_timer`. The service exposes unary
Status, server-streaming Logs, and bidirectional Upload. The firmware and
Tonic-based espctl client share the [Management schema](../../protocol/management.proto)
and [OTA contract](../ota/protocol.md).

## Configuration and memory

| Setting | Default and valid range | Cost and caller responsibility |
| --- | --- | --- |
| HTTP/2 concurrent streams | Two per connection. `Config::max_concurrent_streams` is a `NonZeroU32` (1 through 4,294,967,295). The server advertises and enforces this limit. | Each admitted stream adds transport state, buffers, and handles. Select a limit that fits the executor and heap budget. |
| Inline handler slots | Two in the convenience entry points. The const-generic streaming adapters require `1 <= max_concurrent_streams <= SLOTS`. Invalid capacity fails before handshake. The buffered HTTP adapter has two slots and serializes calls to its mutable service. | Each connection reserves an inline array of `SLOTS` optional handler futures, including empty slots. Measure the concrete future size on the target when changing the service or slot count. |
| HTTP/2 frames and receive flow control | Fixed incoming payload limit of 16 KiB. The initial receive windows are 65,535 bytes each for the connection and for each stream. Outbound capacity depends on peer settings and window updates. | The connection window bounds outstanding flow-controlled bytes shared by all streams. Frame scratch, headers, vector capacity, and application copies add to that budget. Drop a received `Chunk` promptly to allow credit to return. |
| HTTP/2 write queue | One pending command per stream. `send_data` enqueues up to 16 KiB per call and returns the number of bytes accepted. | Retry the remaining slice after a partial write. Await `flush` for transport flush or finish with trailers. A stalled peer can retain queued DATA while it withholds credit. |
| HTTP/2 headers | Fixed 4 KiB compressed block and 4 KiB header-list limits, and a 4 KiB inbound HPACK dynamic-table limit. The outbound encoder uses a zero-sized dynamic table and also observes the peer's header-list limit. | Header-list accounting includes 32 bytes per field in addition to the name and value lengths. Decoded fields and HPACK storage consume heap separately from DATA. |
| Buffered HTTP bodies | The buffered service adapter caps complete request and response bodies at 16 KiB each. | Whole bodies occupy memory in addition to the driver's buffers. Use the stream adapter for incremental work and bound application buffering. |
| gRPC messages | Each typed call takes a const `MAX` in protobuf payload bytes, excluding the five-byte gRPC envelope. The wire length is an unsigned 32-bit field and zero-length messages are valid. Management uses 4,608 bytes per message in both directions. The OTA chunk payload is at most 4,096 bytes. | Choose a bound that accommodates the encoded schema and fits the heap and transport budget. The decoder assembles one message across DATA boundaries. Encoding uses a `MAX`-byte scratch array before producing an owned response buffer. Decoded application values can coexist with receive buffers. The bound applies per message, so applications must manage stream length separately. |
| Firmware connections | Two connection loops, each with two handler slots and 1 KiB TCP RX plus 1 KiB TCP TX. | The socket arrays total 4 KiB across both connections, plus executor, network, HTTP/2, and service state. The demo firmware configures 100 KiB of heap in two regions. Aggregate free bytes may exceed the largest contiguous allocation. |

The protocol limits describe bytes on the wire or in logical buffers. To size
firmware memory, account for allocator capacity, simultaneous connections,
and live handler futures. Admission still allocates stream state with
`Rc::new`, so the fallible buffer reservations do not cover all heap pressure.

## Deadlines and lifecycle

- The streaming runtime accepts one optional `grpc-timeout` field with one
  to eight decimal digits (0 through 99,999,999), followed by `H`, `M`, `S`,
  `m`, `u`, or `n` for hours through nanoseconds. Duplicate or malformed
  values return `InvalidArgument`. Omission leaves the RPC without a
  client-supplied deadline.
- `serve_streaming_with_timer` takes a relative `core::time::Duration` and
  expects a sleep future that wakes when it expires. The runtime polls
  the timer at deadline-aware call operations. Services doing their own
  asynchronous work should use those operations, such as
  `ServerStreaming::start` for setup. The convenience streaming entry
  points validate timeout syntax and use a pending timer.
- Firmware rounds the timer up to whole microseconds and applies separate
  limits: 10 seconds of TCP inactivity, 180 seconds for serving an accepted
  connection, and five seconds without Upload watchdog activity. Watchdog
  expiration ends the connection and dropping the Upload releases its
  flash lease. After activation starts, the firmware gives the driver two
  seconds to send the Commit response, then allows up to 500 ms for a
  best-effort TCP flush before reset.

## Public entry points

| Surface | Repository use and caller contract |
| --- | --- |
| `embedded_http2::server` | The stream-handle API splits the driver from incoming requests. Poll `Connection::run` concurrently with accepting and handling requests on one local executor. Reads must be cancellation-safe. Each stream stores one cancellation waker. Use one cancellation observer per stream. |
| `embedded_http2::service` | The streaming helpers drive the socket and inline handlers together. The buffered helpers gather full bodies and support the buffered gRPC server. |
| `embedded_http2::Server` | The frame-event API exposes explicit receive-credit and response operations. Sequential head/body and buffered convenience methods overlap with the stream-handle APIs. Host transport tests still exercise these paths. |
| `embedded_grpc` | Use generated dispatch with the timed streaming entry point for Management. The buffered `Service` and `serve` entry point support unary calls and remain covered by host interoperability tests. Generated servers currently implement both service traits. |
| Transport types | HTTP/2 exposes ZeroDDS `HeaderField`. gRPC re-exports HTTP/2 `Config` as `Http2Config` and exposes HTTP/2 `StreamError` in its streaming trait. Connection errors have a gRPC wrapper with an `is_disconnected` predicate. |

Keep the overlapping entry points and re-exports compatible during this
audit. Any future narrowing should account for the host fixtures, generated
dispatch, and downstream callers of the public crates.

## Comparison with Tonic 0.14.9

The generated `ManagementServer::new(service)`, typed RPC methods, and
`Result<_, Status>` follow Tonic's service model. The Management wire
contract already interoperates with Tonic and espctl.

| RPC | Tonic generated trait | Embedded generated trait |
| --- | --- | --- |
| Status | `Request<StatusRequest>` → `Response<StatusResponse>` | Plain request and response messages |
| Logs | `Request<ReadLogsRequest>` → `Response<Self::LogsStream>`, with a named associated stream type | Plain request and an opaque stream that can borrow the service |
| Upload | `Request<Streaming<UploadRequest>>` → `Response<Self::UploadStream>` | `Request<Streaming<UploadRequest, MAX>>` → `Response<impl Stream<Item = Result<UploadAck, Status>>>` |

Tonic's request and response wrappers carry metadata and extensions. The
embedded Upload wrappers carry only the typed payload. Status and Logs
pass plain messages, and all three methods use a limited status enum. The
embedded runtime's local futures can borrow connection state. Tonic also
supports client-streaming RPCs with unary replies.

| Concern | Tonic server API | Embedded equivalent |
| --- | --- | --- |
| HTTP/2 stream admission | `Server::max_concurrent_streams` with no explicit limit by default | `Http2Config::max_concurrent_streams` with a required nonzero limit, defaulting to two |
| Handler concurrency | `Server::concurrency_limit_per_connection` independently limits inbound service calls | `SLOTS` reserves inline memory and must accommodate the advertised transport limit |
| Flow control | `initial_stream_window_size` and `initial_connection_window_size` configure the windows | Fixed initial windows with the same 65,535-byte defaults |
| Message bounds | `max_decoding_message_size` and `max_encoding_message_size` configure independent limits, defaulting to 4 MiB and `usize::MAX` respectively | A generated const bound applies in both directions |
| Timeouts | `Request::set_timeout` writes `grpc-timeout`. `Server::timeout` adds a server-side request time limit. | `serve_streaming_with_timer` enforces the client header at deadline-aware operations. Firmware's connection timeout applies to the entire connection. |

Sources for these comparisons are Tonic's [generated server template](https://github.com/hyperium/tonic/blob/v0.14.9/tonic-build/src/server.rs),
[transport server configuration](https://docs.rs/tonic/0.14.9/tonic/transport/server/struct.Server.html),
[gRPC server configuration](https://docs.rs/tonic/0.14.9/tonic/server/struct.Grpc.html),
and [request wrapper](https://docs.rs/tonic/0.14.9/tonic/struct.Request.html).

Upload follows Tonic's ownership shape. The embedded runtime polls the
outbound stream only after the previous reply flushes, and dropping the
stream on reset, disconnect, or deadline expiry releases the flash lease.
The incoming `Streaming<T, MAX>` implements `Stream` and offers
`message().await` with the generated decode bound.

For closer parity on Status and Logs, typed wrappers could carry supported
metadata. If message bounds become public knobs, use Tonic's directional
names and preserve the generated embedded bounds as allocation ceilings.

## Protobuf generation

Cargo runs `crates/management-grpc/build.rs` on the host to read the
Management descriptor and emit the service trait, server, and dispatch into
`OUT_DIR`. The `no_std` bindings use micropb types from `ota-protocol`.

The host-only `embedded-grpc-build` crate requires a Rust const expression
for the message bound and a type mapping. `generate_with_packages` maps
top-level protobuf messages to Rust modules. `generate` accepts a resolver
for fully qualified protobuf names. The caller must supply Rust paths to
compatible micropb types and a const expression in scope in the generated
code. An unmapped message or a client-streaming method with a unary response
fails generation. Management maps the `ota` package to `ota_protocol::ota_`
and passes `ota_protocol::UPLOAD_FRAME` as the bound.

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

The follow-up host run added streaming request validation cases for HTTP method,
content type, and missing, truncated, or extra gRPC messages. Combined runtime
line coverage rose to 96.37% (1,804 / 1,872), region coverage to 93.43%, and
streaming module line coverage to 90.96% (171 / 188). LLVM reported the same
seven mismatched functions.
