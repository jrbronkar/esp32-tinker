# LAN discovery

```sh
cargo run -p espctl -- discover
cargo run -p espctl -- discover --watch --interface LOCAL_IPV4
```

Discovery resolves IPv4 `_espctl._tcp.local.` services using `mdns-sd`; firmware
advertises through `edge-mdns` on UDP 5353. The client follows SRV ports and A
records. TXT contains id, application, firmware, state, OTA availability and
`transport=grpc`. Advertised state is cached metadata; query status for live data.

The default scan is three seconds (`--timeout-ms` accepts 1–60000). Watch mode
pauses three seconds between scans. The client validates bounded metadata,
deduplicates/replaces endpoints, handles removal and displays at most 64 devices.
Firmware records have a 60-second TTL and are refreshed on state/address changes.

Multicast filtering, LAN isolation and routing can hide devices; an empty scan
does not prove a board is offline. Delivery is intermittent on one tested WLAN.
Multiple-board behavior, hostname conflicts and DHCP changes need further
hardware coverage. mDNS records are unauthenticated.

On macOS, inspect records independently with:

```sh
dns-sd -B _espctl._tcp local.
dns-sd -L INSTANCE _espctl._tcp local.
```

See [CLI usage](../../tools/espctl/README.md) and [validation](../ota/testing.md).
