# Embedded HTTP/2 request streaming

The replacement transport currently supports one active stream per connection.
Requests can be read frame by frame. Responses are buffered and sent after the
request half-closes. The production endpoint still uses Protolink.

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

A DATA slice can contain part of a gRPC message or several messages. The gRPC
layer will assemble messages across DATA boundaries. `Upload` also needs incremental responses so the server can
send an ACK while the client keeps its request open. Concurrent `Logs` and
`Upload` on one connection require multiplexing. [gRPC over HTTP/2](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md),
[OTA protocol](../ota/protocol.md).
