# Embedded HTTP/2 streaming

The replacement transport currently supports one active stream per connection.
Requests and responses can transfer one frame at a time. The production
endpoint still uses Protolink.

## Reading a request

1. Call `accept_head` to receive the initial headers and stream ID without waiting
   for the body. `end_stream` is set when the request ends in those headers.
2. Call `read_body(stream_id)` to receive a borrowed DATA slice or an owned
   trailer block. Process the DATA before making another call on the
   connection. A successful read returning `None` means the request has ended.
3. Call `release_capacity(stream_id)` after processing DATA to return all
   credit accrued since the last release. Release credit before waiting for
   more DATA if the peer could have exhausted its window.

HTTP/2 charges both the stream and connection windows for the entire DATA
payload, including padding. While the request is open, releasing credit
updates both windows. After the request half-closes, release updates only the
connection window. `respond` returns any remaining credit before sending the
response. If the peer resets the stream, the server restores outstanding
connection credit before reporting the reset so the next request can proceed.
[RFC 9113 §6.1](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.1),
[RFC 9113 §6.9](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.9).

## Sending a response

1. Call `start_response` to send response headers. The peer can keep sending
   request DATA while the server sends response DATA.
2. Call `send_data` with the remaining response bytes. It sends at most one
   frame, bounded by both send windows and the frame limit. Advance the cursor
   by the returned byte count.
3. If a nonempty slice sends zero bytes, call `wait_send_capacity`. Retry the
   write on `Ready`. If it yields `RequestBody`, process the frame and release
   any consumed DATA credit before waiting again.
4. After the request half-closes, call `finish_response` to send trailers or an
   empty DATA frame with `END_STREAM`.

A DATA slice can contain part of a gRPC message or several messages. The gRPC
layer will assemble messages across DATA boundaries. Incremental responses let
`Upload` send ACKs while the request is open. Concurrent `Logs` and `Upload`
on one connection require multiplexing.
[RFC 9113 §5.1](https://www.rfc-editor.org/rfc/rfc9113.html#section-5.1),
[RFC 9113 §6.9](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.9),
[gRPC over HTTP/2](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md),
[OTA protocol](../ota/protocol.md).

## HTTP/2 conformance baseline

[h2spec](https://github.com/summerwind/h2spec) probes a running HTTP/2 server.
The host fixture uses the same `embedded-http2` connection and service adapter
as the prototype. It listens on loopback with an ephemeral port and responds
with HTTP 200 and a nonempty body, as h2spec expects.

Start the fixture from the repository root in one terminal.

```sh
nix develop --command cargo run --locked -p embedded-http2 --example h2spec-server
```

In another terminal, enter the port printed by the fixture and run the suite.

```sh
printf 'Fixture port: '
read -r port
nix shell --inputs-from . nixpkgs#h2spec --command h2spec -h 127.0.0.1 -p "$port"
```

Pass a section such as `http2/6.9` after the port argument to narrow a rerun.
Stop the fixture with Ctrl-C. The suite exits nonzero when probes fail.

The baseline on `e203612` plus the fixture used h2spec 2.6.0 over cleartext on
macOS. It ran **146 probes, with 120 passing and 26 failing**. This baseline
records the current implementation gaps.

| Failing area | Count | Observed behavior |
| --- | ---: | --- |
| Request fields and message length | 15 | Requests with malformed headers or mismatched `content-length` received an ordinary response. See [RFC 9113 §8.1.1](https://www.rfc-editor.org/rfc/rfc9113.html#section-8.1.1) and [§8.3](https://www.rfc-editor.org/rfc/rfc9113.html#section-8.3). |
| Frame size, flow-control overflow, and unexpected CONTINUATION | 5 | The connection reset before the probe received the expected HTTP/2 error frame. See [RFC 9113 §4.2](https://www.rfc-editor.org/rfc/rfc9113.html#section-4.2), [§6.9.1](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.9.1), and [§6.10](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.10). |
| GOAWAY and invalid preface | 3 | The probe observed a TCP reset. RFC 9113 allows omitting GOAWAY for an invalid preface. See [§3.4](https://www.rfc-editor.org/rfc/rfc9113.html#section-3.4) and [§6.8](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.8). |
| Legacy priority | 2 | Self-dependency probes expected the RFC 7540 error behavior. Review these against the deprecation of priority signaling in [RFC 9113 §5.3](https://www.rfc-editor.org/rfc/rfc9113.html#section-5.3). |
| Multiple CONTINUATION frames | 1 | The probe sends two 4,000-byte header values, exceeding the fixture's 4 KiB header-list limit. All six cases in the section passed on rerun with `--max-header-length 512 http2/6.10`. [Probe source](https://github.com/summerwind/h2spec/blob/v2.6.0/http2/6_10_continuation.go). |

h2spec targets RFC 7540 and RFC 7541. Use RFC 9113 to assess its HTTP/2
findings. [Upstream RFC 9113 tracking issue](https://github.com/summerwind/h2spec/issues/128). The Rust `h2` and Tonic tests cover the transport and RPC behavior
specific to this project.

### CI exceptions by case

`nix run .#http2-conformance` runs all 146 default probes. The
[case list](../../tools/http2-conformance/known-failures.txt) allows the
following failures. Any other failed probe, skipped case, or incomplete run
fails the check. A passing exception is reported so it can be removed.

| Case | What the probe expects | Why CI allows the default run to fail |
| --- | --- | --- |
| `generic/3.8/1` | Accept peer GOAWAY and observe a connection close or PING ACK. | The fixture drops the socket on GOAWAY. With unread data, the peer can observe a TCP reset. Revisit during graceful shutdown work. [RFC 9113 §6.8](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.8) |
| `http2/3.5/2` | Terminate the connection for an invalid preface. The probe accepts a protocol-error GOAWAY or a clean close. | The fixture closes and the peer can see a TCP reset. The RFC permits omitting GOAWAY for an invalid preface. [RFC 9113 §3.4](https://www.rfc-editor.org/rfc/rfc9113.html#section-3.4) |
| `http2/7/1` | Treat GOAWAY with an unknown error code like an ordinary GOAWAY. | The same socket teardown can produce a TCP reset. Revisit alongside the ordinary GOAWAY case. [RFC 9113 §6.8](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.8), [§7](https://www.rfc-editor.org/rfc/rfc9113.html#section-7) |
| `http2/5.3.1/1` | Signal a protocol error for a HEADERS priority dependency on the same stream. | The server skips the legacy priority fields and processes the headers. RFC 9113 deprecates the RFC 7540 dependency scheme. [RFC 9113 §5.3.2](https://www.rfc-editor.org/rfc/rfc9113.html#section-5.3.2) |
| `http2/5.3.1/2` | Signal a protocol error for a PRIORITY frame that depends on its own stream. | The server ignores legacy priority signals after checking frame shape. [RFC 9113 §5.3.2](https://www.rfc-editor.org/rfc/rfc9113.html#section-5.3.2), [§6.3](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.3) |
| `http2/6.10/1` | Accept a header block split across multiple CONTINUATION frames. | Two default 4,000-byte field values exceed the 4 KiB advertised header-list limit. CI requires this exact probe to pass separately with `--max-header-length 512`. [RFC 9113 §6.10](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.10) |
| `http2/6.10/5` | Receive GOAWAY with `PROTOCOL_ERROR` for an extra CONTINUATION after `END_HEADERS`. | The server sends GOAWAY, but the probe can see a TCP reset before receiving it. Keep this exception until error delivery is reliable. [RFC 9113 §6.10](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.10) |
