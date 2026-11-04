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

### Multicast delivery investigation

Discovery remains intermittent on the current LAN. Temporary counters at the
Wi-Fi-driver and DNS-socket boundaries isolated the failure before the IP stack:

| Controlled observation | Result |
| --- | --- |
| Two failed scans | Zero incoming mDNS frames at either board boundary |
| Next successful scan | One incoming frame at each boundary, followed by a response |
| Explicit multicast query with a unicast reply expected | Timed out without increasing board receive counters |
| Unicast query from port 5353, triggering a multicast response | Mac received the board response |
| Direct DNS queries | Continued to return identity and diagnostic counters |

The driver handed an IGMP membership report to the radio; this does not establish
that the AP received or accepted it. These observations point to intermittent
multicast delivery toward the ESP32, rather than service resolution or DNS
response parsing. They do not distinguish Mac transmission, AP forwarding, and
radio filtering before the driver boundary. Host socket loopback confirms query
generation, not over-the-air delivery. Power saving is disabled by default in
the installed radio driver. Changing host library versions did not resolve it.

A subsequent macOS en0 capture showed nine valid multicast PTR queries across
three failed scans, with no board replies during those scans. The same capture
received the board startup multicast announcement and a direct DNS reply about
188 ms after its request. Query checksums and IP TTL 255 were correct. This
confirms query output at the Mac interface, but not physical radio delivery.

Next, compare on another LAN/AP if available, or capture at the AP/wireless
boundary to distinguish forwarding from radio reception. macOS packet capture
requires administrator access. Keep packet captures and temporary diagnostic firmware outside the
repository; the production TXT contract excludes these counters.
