# gRPC management implementation

`device-demo` exposes one `esp.experimental.Management` service on TCP 7779.
Protolink is pinned in `Cargo.toml`; `Cargo.lock` pins its HTTP/2 dependencies.
Handlers use bounded micropb messages. The build generates RPC paths from
`management.proto` and checks the signatures against the Rust handlers.
See the [protocol contract](../ota/protocol.md).

| RPC | Shape | Request → response |
| --- | --- | --- |
| Status | Unary | `StatusRequest` → `StatusResponse` |
| Logs | Server streaming | `ReadLogsRequest` → stream of `ReadLogsResponse` |
| Upload | Bidirectional streaming | stream of `UploadRequest` → stream of `UploadAck` |

## Bounds

| Resource | Limit |
| --- | --- |
| Connections / active streams per connection | 2 / 2 |
| Flash leases / pending ACKs per upload | 1 / 1 |
| Stream / connection receive window | 8192 / 65535 bytes |
| Decoded headers / pending output per connection | 1024 / 8192 bytes |
| Upload / status-log request body | 4608 / 512 bytes |
| Response body | 512 bytes |
| Retained logs / batch size | 32 / 4 records |

Compression is disabled. The Embassy driver polls TCP halves independently.
Connections share one flash lease and have separate upload watchdogs and reboot
ownership. A second client can query status during an upload.

`LogStore` combines retained storage with Embassy bounded multi-waker
registration. Snapshots and follow mode use the same stream; cursors carry
boot identity and overwritten-history information. Typed lifecycle events control
watchdog, state and reboot behavior; log text is presentation.

For commands, use the [CLI guide](../../tools/espctl/README.md).
For evidence and production limits, see [testing](../ota/testing.md) and
[architecture](../ota/design.md#production-gaps).
