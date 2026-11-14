# Embedded HTTP/2

The firmware uses the `embedded-http2` stream-handle driver. See the
[management guide](embedded-grpc.md) for server entry points and limits.

## Flow control

The stream-handle API returns receive credit after the buffered and held
DATA drains. The low-level `Server` API exposes explicit credit accounting.
Consume borrowed DATA before advancing the connection and call
`release_capacity` before the peer exhausts its window. If a response
write returns zero, wait for send capacity. Process any request DATA
returned by `wait_send_capacity` and release its credit before waiting
again.

HTTP/2 charges both receive windows for the entire DATA payload, including
padding. After the request half-closes, returning credit updates only the
connection window. A reset restores the stream's outstanding connection
credit so other streams can proceed. See
[RFC 9113 §6.1](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.1) and
[§6.9](https://www.rfc-editor.org/rfc/rfc9113.html#section-6.9).

## HTTP/2 conformance baseline

[h2spec](https://github.com/summerwind/h2spec) probes a running HTTP/2 server.
The host fixture uses the same `embedded-http2` connection and service adapter
as the firmware. It listens on loopback with an ephemeral port and responds
with HTTP 200 and a nonempty body, as h2spec expects.

Start the fixture from the repository root in one terminal.

```sh
nix develop --command cargo run --locked -p embedded-http2 --example h2spec-server
```

In another terminal, enter the port printed by the fixture and run the suite.

```sh
printf 'Fixture port: '
read -r port
nix shell --inputs-from . nixpkgs#h2spec --command h2spec -h 127.0.0.1 -p "$port"
```

Pass a section such as `http2/6.9` after the port argument to narrow a rerun.
Stop the fixture with Ctrl-C. The suite exits nonzero when probes fail.

The initial baseline on `e203612` plus the fixture passed 120 of 146
probes with h2spec 2.6.0. The CI baseline below tracks the current exceptions.

h2spec targets RFC 7540 and RFC 7541. Use RFC 9113 to assess its HTTP/2
findings. [Upstream RFC 9113 tracking issue](https://github.com/summerwind/h2spec/issues/128). The Rust `h2` and Tonic tests cover the transport and RPC behavior
specific to this project.

CI runs `nix run .#http2-conformance` and checks all 146 probe results against the six documented exceptions in
[the case list](../../tools/http2-conformance/known-failures.txt). It uses
`--max-header-length 512` for the valid CONTINUATION probe so its headers fit
within the advertised limit. The remaining probes use their default inputs.
