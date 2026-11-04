# mDNS / DNS-SD device discovery

Find ESP32 management services on the same IPv4 LAN without a board IP or
management-port assumption:

```sh
nix develop
cargo run -p espctl -- discover
cargo run -p espctl -- discover --watch
```

The board advertises `_espctl._tcp.local.` using `edge-mdns`; the Mac browses and
resolves services using `mdns-sd`. No custom DNS encoder, Protobuf discovery
message, private UDP port, or IP sweep is used. The existing TCP management API
is unchanged. Discovery-enabled `device-demo` firmware is required; sensor demos
and older firmware do not advertise this service.

## Advertised records

| Record | Purpose |
| --- | --- |
| PTR | Find instances of `_espctl._tcp.local.` |
| SRV | Resolve the target hostname and actual management port |
| A | Resolve the board's current IPv4 address |
| TXT | Small application metadata: id, application, firmware, protocol, state, ota |

Hostnames derive from the board's base MAC, such as
`esp32s3-a4cb8fd813c8.local.`. Discovery follows SRV/A records, not a hardcoded
management endpoint. The browser supports protocol=1 and validates bounded ID
and firmware metadata. The responder uses fixed UDP and DNS buffers and sets IP
TTL to 255. It rebuilds its address records when the network configuration changes.

State changes request fresh announcements. Values are idle, receiving, verified,
and committing, matching [the management lifecycle](../ota/protocol.md).
TXT is cached advertised metadata, not a guaranteed live health check. For live
uptime and boot details, query the resolved endpoint explicitly:

```sh
cargo run -p espctl -- status --host <resolved-ip> --port <resolved-port> --boot
```

## Mac interfaces and collection

The default browser searches available interfaces and displays resolved IPv4 endpoints. To restrict it to a specific
interface, supply that interface's local IP address:

```sh
cargo run -p espctl -- discover --interface 10.128.248.55 --timeout-ms 5000
```

The default collection window is three seconds, plus bounded daemon startup and
shutdown overhead. Watch mode waits three seconds between scans. Ctrl+C stops
it. The application deduplicates service instances/endpoints, applies replacement
and removal events, caps displayed endpoints at 64, and reports overflow. DNS
packet processing, caching, TTLs, and retransmissions belong to the library;
this display cap is not a total memory bound for the host library's cache.

macOS Bonjour can inspect the same service independently:

```sh
dns-sd -B _espctl._tcp local.
dns-sd -L <instance-name> _espctl._tcp local.
dns-sd -G v4 <hostname>.local.
```

Multicast DNS uses the standard 224.0.0.251:5353 address/port. Firewalls, AP client
isolation, multicast filtering, and routing can hide services; an empty scan is
not proof that boards are offline. Cross-subnet discovery and persistent inventory
are outside this implementation. MAC-derived names reduce collisions; automatic
hostname conflict renaming has not been validated. TXT metadata is unauthenticated.

## Validation and scope

Host tests cover resolved SRV ports (including non-default ports), TXT metadata,
address replacement, deduplication, removal, invalid records, result limits, and
CLI options. Multicast interoperability needs actual LAN tests; fixture tests do
not establish it. See [current hardware validation](../ota/testing.md#validation-status).
Remote logs and fleet inventory remain separate future work.

The current LAN test is intermittent: direct DNS queries succeed, but repeated
multicast scans sometimes miss the board in both espctl and native Bonjour.
Multicast delivery needs further investigation before treating discovery as
reliable. Changing host library versions did not resolve this behavior.
