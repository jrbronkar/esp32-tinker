# LAN device discovery

Find ESP32 boards running `device-demo` on the same IPv4 LAN:

```sh
nix develop
cargo run -p espctl -- discover
cargo run -p espctl -- discover --watch
```

The output lists each responder's device ID, management address, firmware version,
management state, OTA availability, and uptime. Use the discovered address for
normal status/upload commands. Sensor demos do not yet start this service, and
firmware predating discovery will not respond.

## Interfaces and network limits

By default the client sends IPv4 limited broadcasts to 255.255.255.255, UDP port
7780, on the OS-selected route. To select a LAN explicitly:

```sh
cargo run -p espctl -- discover \
  --broadcast 10.128.248.255 --bind 10.128.248.55 --timeout-ms 4000
```

Replace these examples with your subnet's broadcast and the Mac's local address.
The discovery window defaults to two seconds; probes repeat every 500 ms. Watch
mode waits three seconds between completed scans and prints only that scan's
responders. Ctrl+C stops it. No responses is a successful empty scan, not proof
that devices are offline: packet loss, firewalls, client isolation, routing, and
sleeping devices can hide them. There is no cross-subnet discovery or IP sweep.
This is a small Protobuf discovery protocol, not mDNS/Bonjour.

## Wire contract

- Client binds an ephemeral UDP socket and sends `Request` with protocol version
  1, a scan correlation ID, and the `discovery` payload. One datagram contains the
  Protobuf body directly, without the TCP length prefix.
- Firmware accepts only this operation/version on UDP 7780. Invalid packets and
  other operations elicit no reply. It replies by unicast to the sender address
  and port with the existing `Response.status` envelope and matching ID.
- Both bodies are bounded to 512 bytes. The reply omits boot inventory and timing
  diagnostics. The management TCP port remains 7779.
- The firmware caps responses at five per second. Socket buffers and task storage
  are fixed-size. The host deduplicates by device ID plus address, keeps the latest
  received snapshot, caps results at 64, and reports if further devices were omitted.
- The host ignores malformed, oversized, wrong-version, wrong-ID, wrong-source-port,
  wrong-payload, non-unicast-source, and empty-identity replies. A scan ID is correlation, not authentication.

The UDP task shares the existing Embassy stack but is independent of the single
TCP listener. It can answer while an upload connection is active, subject to
executor scheduling and synchronous flash operations delaying other tasks.

## Management state

StatusResponse field 9, `management_state`, is an additive string capped at 16
bytes. Empty means unavailable on older firmware. Current values are:

| Value | Meaning |
| --- | --- |
| `idle` | No upload session is active |
| `receiving` | Begin accepted; receiving or validating candidate data |
| `verified` | Stored image validated; awaiting Commit or Abort |
| `committing` | Boot metadata commit started; reset follows success |

A dropped/failed/aborted/timed-out upload returns to idle through a guard, even
when the session future is cancelled. State is an instantaneous management
snapshot, not a sensor-health or rollback guarantee. It is not persistent.

## Validation

The host suite includes real-UDP mock tests for multiple devices, deduplication,
invalid replies, dropped initial probes, collection deadlines, CLI empty output,
and the 64-device cap. Protocol tests verify that discovery strips inventory,
echoes IDs, rejects unsupported operations/versions, and fits the control bound.
On one real ESP32-S3, default and explicit-subnet discovery returned the unicast
address. Discovery reported receiving and verified while the TCP listener was
occupied, then idle after session close. Multiple-device handling is mock-tested,
not yet a physical multi-board result. Hardware validation is summarized in
[OTA testing](../ota/testing.md#validation-status).

Discovery and management remain unauthenticated LAN development services. These
are observations from responding peers, not cryptographically verified identities.
Remote logs and fleet inventory are separate future work.
