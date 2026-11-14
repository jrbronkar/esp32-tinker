# Management protocol

`esp.experimental.Management` uses cleartext gRPC/HTTP2 on TCP 7779.
The [service](../../protocol/management.proto) and [messages](../../protocol/ota.proto)
are the API contract; see [RPC types and bounds](../device-management/embedded-grpc.md).
Errors use gRPC status trailers. Only Upload uses operation variants and request
IDs; status and log RPCs carry their declared messages directly.

## Upload lifecycle

1. Begin declares application, chip, firmware version, size and SHA-256, validates
   metadata and acquires the exclusive flash lease.
2. Chunk writes the next 4096-byte sector or final tail to the inactive slot.
   IDs increase by one. Await each ACK and validate ID, phase, offset and target.
3. Finish reads flash back and checks SHA-256, ESP image structure, descriptor,
   embedded version, segment checksum and hash footer.
4. Commit explicitly selects the verified image and resets. Activation is latched
   before metadata writes; further writes remain blocked until reset, even after
   a metadata error. The driver allows two seconds to send the ACK/trailers and
   attempts a bounded TCP flush. Disconnect also triggers pending reset.
5. Abort or half-close without Commit releases flash and preserves boot selection.
   A new session starts from Begin; there is no resume or persistent commit token.

A valid Committed ACK establishes success even if reset truncates trailers.
A lost/invalid ACK or metadata failure leaves activation uncertain: inspect the
running firmware and slot before retrying. No transmitted operation is replayed.

Upload and Logs progress independently on one connection. Log cursors are
boot-scoped; resuming a stalled reader reports overwritten history. Only accepted
upload operations refresh the five-second upload watchdog. Expiry closes that
connection and releases flash, regardless of log traffic or HTTP/2 keepalives.
TCP inactivity is limited to ten seconds and connection lifetime to 180 seconds;
RPC deadlines may expire sooner.

This experimental API changes in place. Build client and firmware from the same
schema; use [USB provisioning](provisioning.md) when they cannot communicate.
There is no version handshake or schema fallback.
