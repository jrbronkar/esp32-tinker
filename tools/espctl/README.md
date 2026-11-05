# espctl

Host CLI for `device-demo` management on TCP 7779. Run from the repository root
inside `nix develop`; use `--help` for complete options. Build client and firmware
from the same schema. See [provisioning and recovery](../../docs/ota/provisioning.md).

```sh
cargo run -p espctl -- discover
cargo run -p espctl -- status --host BOARD_IP --boot
cargo run -p espctl -- logs --host BOARD_IP --follow
cargo run -p espctl -- profile --host BOARD_IP --samples 10 --interval-ms 100
cargo run -p espctl -- upload --host BOARD_IP \
  --image IMAGE.bin --firmware-version VERSION --logs --commit
```

Targets use an IP literal, `--port 7779` and `--timeout-ms 5000` by default.
Discovery supports `--watch`, `--interface LOCAL_IPV4` and `--timeout-ms`;
see [DNS-SD details](../../docs/device-management/discovery.md).

## Uploads and logs

Upload accepts an exported application-only `device-demo` binary with the matching
embedded version. Without `--commit`, it writes/verifies the inactive slot and
preserves boot selection. `--logs` streams live logs on the upload's HTTP/2
connection; another client can query the same endpoint concurrently. Total upload
time is limited to 180 seconds. Operations are never replayed. If a commit ACK is
lost, check the running firmware and slot before retrying.

`logs` pages through retained records; `--follow` polls every 500 ms when caught
up and reconnects after network loss. Cursors reset after reboot and warn when
history was overwritten. Output escapes remote terminal control characters.
These are management logs, not a capture of all serial output.

## Profiling

`status --timings` reports TCP connect/retry, encode, RPC response and decode/check
durations. Each profile sample opens a fresh connection. Retry wait is included
in connect; RPC response includes HTTP/2 setup and server/body/trailer time.
Device decode/prepare diagnostics overlap host phases and include preemption.
`profile --csv` emits:

```text
sample,total_ms,connect_ms,retry_wait_ms,encode_ms,rpc_ms,decode_validate_ms,refused_connects,device_decode_us,device_prepare_us
```

Failures produce a nonzero exit; profile CSV includes successful samples only.

## Local validation

```sh
cargo test
cargo run -p espctl --example mock-device -- 127.0.0.1:7779
# In another terminal:
cargo run -p espctl -- status --host 127.0.0.1
```

The mock implements status, not radio or flash. See [hardware probes and limits](../../docs/ota/testing.md),
[protocol](../../docs/ota/protocol.md) and [implementation](../../docs/device-management/protolink.md).
Updates are unsigned and unauthenticated; [production gaps](../../docs/ota/design.md#production-gaps) remain.
