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

## Terminal dashboard

```sh
cargo run -p espctl -- tui
cargo run -p espctl -- tui --host BOARD_IP
```

Run the dashboard in an interactive terminal with a board running the gRPC
management firmware. The dashboard scans the local network on launch. Use Left/Right to switch
boards and R to rescan. Use Up/Down or Page Up/Page Down to scroll the
last 1,000 rows, End to return to live output, F to cycle severity filters,
and Q to quit. Status refreshes every two seconds alongside the live log
stream on the same HTTP/2 connection. After a disconnect, the dashboard
reconnects and resumes from its last cursor, marking device resets and
overwritten history. The stream contains firmware management logs. Use the
existing `upload` command for scripted firmware updates.

If discovery returns an empty list, press R to rescan or connect with
`--host BOARD_IP`. On a Mac with several active network interfaces, use
`--interface LOCAL_IPV4` to select the interface for discovery. A direct
connection accepts `--port` if the board uses a custom gRPC port.

### Update firmware from the dashboard

From the repository root, export `WIFI_SSID` and `WIFI_PASSWORD`, then build
and export the image with the pinned Nix tools:

```sh
nix run .#ota-image
nix run .#espctl -- tui
```

The version defaults to the short Git commit hash, with a second hash when
tracked files have local changes. Pass `-- VERSION` to override it. The export
lands in `target/ota/<version>.bin`. Select a board and press **U** to browse compatible application binaries in `target/ota`. Use **↑/↓** to choose
an image and **Enter** to write and verify the inactive slot. The picker reads
the version from the image. For an image elsewhere, launch with
`tui --image /path/to/app.bin` and press **U**.

The dashboard streams logs on the upload connection and resumes status polling
when the transfer ends. After verification, press **A** to review the board, version, and reboot prompt, then **Y** to activate or
**Esc** to dismiss the prompt. The dashboard releases the first upload session
after verification. Activation resends and verifies the same in-memory image
before committing it. The dashboard checks the running version and slot after
reboot and reports an unknown outcome if it loses the activation response and
cannot confirm the new image. Check the board's status before retrying an
unconfirmed activation.

## Uploads and logs

Upload accepts an exported application-only `device-demo` binary with the matching
embedded version. Without `--commit`, it writes/verifies the inactive slot and
preserves boot selection. `--logs` streams live logs on the upload's HTTP/2
connection; another client can query the same endpoint concurrently. Total upload
time is limited to 180 seconds. Operations are never replayed. If a commit ACK is
lost, check the running firmware and slot before retrying.

`logs` reads retained records on one stream. `--follow` waits for new records
and resumes from its cursor after reconnecting. Cursors reset after reboot and warn when
history was overwritten. Output escapes remote terminal control characters.
These are management logs, not a capture of all serial output.

## Profiling

`status --timings` reports TCP connect/retry, encode, RPC response and decode/check
durations. Each profile sample opens a fresh connection. Retry wait is included
in connect; RPC response includes HTTP/2 setup and server/body/trailer time.
Device preparation timing overlaps host phases and includes preemption. The
generated dispatcher decodes before the handler, so device decode is blank when
unavailable.
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

The `mock-device` example serves status. The host tests also run a loopback
gRPC board with the firmware management handler and RAM-backed flash. To run
the dashboard update boundary test on its own:

```sh
cargo test -p espctl --lib update_requires_confirmation_and_checks_boot_over_grpc
```

The test covers verification, confirmation and cancellation, and boot status
after a successful or lost activation reply. See [hardware probes and limits](../../docs/ota/testing.md),
[protocol](../../docs/ota/protocol.md) and [implementation](../../docs/device-management/protolink.md).
Updates are unsigned and unauthenticated; [production gaps](../../docs/ota/design.md#production-gaps) remain.
