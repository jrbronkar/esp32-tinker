# Management and OTA protocol

Firmware updates use standard HTTP/2 and gRPC on TCP **7780**, implemented by
pinned Protolink. The schema is [`management.proto`](../../protocol/management.proto),
using the fixed-capacity messages in [`ota.proto`](../../protocol/ota.proto).
See [implementation, limits and validation](../device-management/protolink.md).
The `esp.experimental` service name is retained for client compatibility.

## RPCs

| RPC | Shape | Payload |
| --- | --- | --- |
| Management/Status | Unary | Status request and current identity/boot information |
| Management/Logs | Server streaming | One cursor request; batches of up to four retained records |
| Management/Upload | Bidirectional streaming | Ordered Begin/Chunk/Finish/Commit or Abort operations and ACKs |

Bodies use protocol version 1 and echoed request IDs. gRPC owns message framing,
HTTP/2 routing, stream flow control and status trailers; no legacy four-byte
length prefix is placed inside a gRPC body. Messages are bounded to 4608 bytes,
and responses to 512. Firmware streams 4096-byte sectors into the inactive slot;
it never buffers a complete firmware image.

## Upload lifecycle

1. Begin declares application, chip, embedded version, size and SHA-256. It
   acquires the sole flash lease after validation and returns Receiving at zero.
2. Chunk supplies exactly the next offset and one sector (or the final tail).
   ACKs contain the new offset and fixed inactive target. IDs increase by one.
3. Finish reads actual flash back, verifies SHA-256 and the ESP image structure,
   descriptor, embedded version, segment checksum and image hash footer.
4. Commit is accepted only in that verified session. It latches activation,
   blocks any more writes, updates boot metadata and returns Committed. The
   firmware allows a two-second response grace period, flushes TCP best effort
   and resets; a disconnect during pending activation also resets.
5. Abort or half-close without Commit releases the lease and preserves boot
   selection. Written bytes remain in the inactive slot; a new upload must start
   from Begin. There is no cross-session commit token or resume facility.

Clients await each ACK before the next operation and validate version, ID,
phase, next offset and target. No transmitted operation is automatically replayed.
A valid Committed ACK is success even if reboot truncates trailers. If it is lost
or invalid, activation is uncertain: reconnect and inspect firmware/slot before
retrying. A metadata error can also leave selection partially changed, so writes
remain blocked until reboot after any actual commit attempt.

## Concurrency and deadlines

Upload and Logs progress on one HTTP/2 connection. A stalled log stream cannot
consume the upload's stream window; bounded retention reports overwritten
history when reads resume. Log cursors are boot-scoped and cancellation frees
subscription state.

A five-second upload inactivity watchdog is refreshed only by accepted upload
operations, not log messages, HTTP/2 PINGs or other TCP traffic. Expiry closes
that connection, cancels its RPCs and releases flash. Completed/aborted uploads
clear the watchdog; pending activation uses the reboot path instead.
The connection also has a ten-second TCP inactivity timeout and a 180-second
absolute lifetime. Client-requested gRPC deadlines can expire sooner.

## Read-only TCP endpoint

TCP **7779** remains for status, profiling and log polling. A four-byte big-endian
length precedes a body of 1–512 bytes. Only Status and ReadLogs are served;
Begin, Chunk, Finish, Commit and Abort are rejected as InvalidRequest. The
listener has no flash target reference and cannot dispatch an OTA writer.
The old upload transport, CLI `--legacy` and its manual harness are removed.

## Discovery and limits

DNS-SD `_espctl._tcp.local.` advertises the read-only status port, identity,
firmware, state, OTA availability, `upload=grpc` and `grpc_port=7780`.

Image layout/bounds and boot selection are unchanged by the transport migration.
Unsigned development updates remain unauthenticated and unencrypted. TLS,
signatures, automatic rollback and dependable power-loss recovery are unverified.
