# Experimental Protolink transport

The `experimental-grpc` feature on `device-demo` adds cleartext HTTP/2 on TCP
7780. The legacy management/OTA service remains on 7779 and retains sole flash
ownership. Discovery still advertises the legacy endpoint only.

Protolink is pinned to upstream revision
`62cc2c87a687ba65fc6885e7c9691cefacf2d064` (0.0.5). Cargo.lock also pins its
HTTP/2/HPACK dependencies, currently zerodds 1.0.0-rc.7. Its default micropb
feature is disabled: the raw Handler uses existing micropb 0.7 protobuf bodies,
without the legacy four-byte transport length prefix.

## Implemented

`/esp.experimental.Management/Status` takes `ota.Request` with protocol version
1 and a status payload, returning `ota.Response`. See
`protocol/management.proto`. The firmware updates uptime and management state
at dispatch. The host fixture tests this Handler over a real TCP connection
against Tonic's independent HTTP/2/gRPC implementation, including eight calls
on one connection, malformed protobuf and unknown-method status trailers.

The Embassy listener uses `server_split` on TCP socket halves and an Embassy
clock/timer for deadlines. Connections have a ten-second socket inactivity
timeout and 180-second absolute lifetime, and the listener rearms after failure.
Two streams are permitted, each with a 1024-byte receive window; decoded headers
are capped at 1024 bytes, serialized output at 4096 bytes and messages at 512
bytes. Compression is disabled. HTTP/2's initial connection window cannot be
lowered below 65535; these settings are not a proof of a total heap bound.

## Validation (2026-11-04)

- Existing default host suite passed before changes.
- Unchanged release firmware build passed.
- Tonic interoperability test passed.
- ESP32-S3 release build with the experimental feature passed.
- Board discovered as `esp32s3-a4:cb:8f:d8:13:c8`, `10.128.248.68:7779`,
  running `network-logs-2`. Experimental firmware has not been activated.

Comparable section sizes, using the same Nix toolchain and release settings:

| Section | Legacy | Experimental | Change |
| --- | ---: | ---: | ---: |
| .data | 11396 | 11804 | +408 |
| .bss | 102956 | 107060 | +4104 |
| .text | 400937 | 490633 | +89696 |
| .rodata | 44924 | 56804 | +11880 |
| .stack (linker remainder) | 184876 | 180364 | -4512 |

Other listed Wi-Fi/RAM code sections were unchanged. The `.stack` section is a
linker reservation, not measured stack usage. The generic `size` total includes
dummy and reserved sections and must not be called actual RAM consumption.
The reserved heap remains 100 KiB; runtime peak/free heap has not been measured.

## Reproduce

Enter `nix develop` at the repository root. Then:

```sh
cargo test -p device-grpc
# Export WIFI_SSID and WIFI_PASSWORD locally, without printing them.
cargo build --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc \
  --release -p device-demo --features experimental-grpc
```

After a separately validated firmware deployment, a grpcurl client can use the
proto files without reflection:

```sh
grpcurl -plaintext -import-path protocol -proto management.proto \
  -d '{"protocolVersion":1,"requestId":1,"status":{}}' \
  BOARD_IP:7780 esp.experimental.Management/Status
```

## Outstanding evaluation

Connect the RAM log buffer to bounded server streaming with wakeups,
cancellation and slow-reader tests. Then integrate OTA through exclusive flash
ownership, ordered sector-sized chunks, verification and explicit activation;
never buffer a complete image. Demonstrate simultaneous upload and logs over
one connection, with flow-control isolation, deadlines and disconnect tests.
Measure runtime heap and task responsiveness on hardware before treating the
transport as viable. TLS/authentication remain a separate step. This endpoint
is an unauthenticated development experiment, and existing rollback and
power-loss guarantees remain unproven.
