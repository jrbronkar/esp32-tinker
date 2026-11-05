# Management and OTA protocol

All management operations use standard HTTP/2 and gRPC on TCP **7779**, implemented by
pinned Protolink. The schema is [`management.proto`](../../protocol/management.proto),
using the fixed-capacity messages in [`ota.proto`](../../protocol/ota.proto).
See [implementation, limits and validation](../device-management/protolink.md).
The service is `esp.experimental.Management`.

## RPCs

| RPC | Shape | Payload |
| --- | --- | --- |
| Management/Status | Unary | Status request and current identity/boot information |
| Management/ReadLogs | Unary | One retained-log snapshot for polling/follow |
| Management/Logs | Server streaming | One cursor request; batches of up to four retained records |
| Management/Upload | Bidirectional streaming | Ordered Begin/Chunk/Finish/Commit or Abort operations and ACKs |

Each RPC uses its declared request/response message directly. Only the Upload
stream uses `UploadRequest` operation envelopes and direct `UploadAck`
messages with request IDs. Errors use gRPC status trailers. gRPC owns framing,
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

Clients await each ACK before the next operation and validate ID,
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

## Single endpoint and discovery

One Management service serves every RPC on TCP **7779**. Two bounded connections
allow a second client to query status during an upload. Connections have independent
idle watchdogs and share one exclusive flash lease. The raw TCP listener and the
separate port 7780 are removed; old length-prefixed clients must be updated.

DNS-SD `_espctl._tcp.local.` advertises this single endpoint using the SRV port,
identity, firmware, state, OTA availability and `transport=grpc`.

Image layout/bounds and boot selection are unchanged by the transport migration.
Unsigned development updates remain unauthenticated and unencrypted. TLS,
signatures, automatic rollback and dependable power-loss recovery are unverified.

## Client and firmware

This experimental gRPC API changes in place. Build the client and firmware from
the same schema. Provision matching firmware over USB when the running device
cannot be managed by the current client. There is no version handshake or schema
fallback. Firmware version, chip/application checks and image validation remain
part of upload admission and verification.
