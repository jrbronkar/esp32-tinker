# LAN discovery

Find provisioned `device-demo` boards on the same LAN without knowing their IP
addresses or management ports:

```sh
nix develop
cargo run -p espctl -- discover
cargo run -p espctl -- discover --watch
```

Use `--interface <local-interface-ip>` to restrict browsing to one interface,
and `--timeout-ms 5000` to extend the default three-second collection window.
Watch mode waits three seconds between scans. Ctrl+C stops it.

## Service contract

Firmware uses `edge-mdns` to advertise `_espctl._tcp.local.`. The CLI uses
`mdns-sd` to resolve IPv4 endpoints from these standard DNS-SD records:

| Record | Meaning |
| --- | --- |
| PTR | Lists service instances |
| SRV | Gives each instance's target hostname and management port |
| A | Resolves the target hostname to its current IPv4 address |
| TXT | Carries id, application, firmware, protocol, state, ota and transport metadata |

Hostnames derive from each board's MAC. The client follows advertised SRV ports
and A records rather than assuming a management endpoint. TXT keys and the
protocol version are shared constants in `ota-protocol`. The SRV port
identifies the single Management service. `transport=grpc` distinguishes it from
older length-prefixed services; the CLI ignores incompatible transport records.

The responder matches query names, record types and classes, including ANY
queries and the mDNS unicast-response class bit. Related A/SRV/TXT records remain
available in the additional section. DNS parsing and serialization belong to the
library; a small wrapper adds type/class matching to its name-based handler.

TXT states are idle, receiving, verified and committing. The service task owns
Runtime; concurrent gRPC connections and mDNS futures borrow it. Upload
completion/cancellation restores idle unless activation has been latched. State changes request fresh announcements.
Fixed-size firmware buffers bound packet storage; records have a 60-second TTL
and unique records use cache-flush semantics. Address records are rebuilt when
DHCP configuration changes.

## Interpreting results

TXT is cached, unauthenticated advertised metadata. Query the resolved endpoint
for current uptime and boot inventory:

```sh
cargo run -p espctl -- status --host <resolved-ip> --port <resolved-port> --boot
```

The CLI validates bounded metadata, deduplicates instance/endpoints, processes
replacement/removal events, and caps displayed endpoints at 64. The host library
owns DNS caching and packet processing; the display cap does not bound its cache.
Each watch iteration creates a new browse session.

mDNS uses the standard multicast address 224.0.0.251 and UDP port 5353. AP
filtering, client isolation and routing can hide services; an empty scan does not
prove a device is offline. Successful resolution also does not establish that
the advertised endpoint is reachable from the client's current IP configuration.

On macOS, Bonjour provides an independent diagnostic client:

```sh
dns-sd -B _espctl._tcp local.
dns-sd -L <instance-name> _espctl._tcp local.
dns-sd -G v4 <hostname>.local.
```

## Validation and limits

Host tests exercise record filtering, SRV ports, metadata, replacement/removal,
deduplication, result limits and CLI options. Physical tests found intermittent
multicast delivery before the board's IP stack on one WLAN; another access point
passed 10/10 discovery scans. The exact AP/radio interoperability cause remains
unresolved. See [validation status](../ota/testing.md#validation-status) for the
latest firmware smoke test and remaining gaps.

Older firmware and sensor demos do not advertise this service. Multiple physical
boards, automatic hostname conflict renaming and DHCP changes remain hardware
coverage gaps. Cross-subnet discovery, remote logs and persistent inventory are
outside this change. Keep captures and diagnostic firmware outside the repo.
