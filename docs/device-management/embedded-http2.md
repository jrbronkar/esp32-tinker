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
