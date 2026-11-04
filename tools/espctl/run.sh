#!/bin/sh
# Avoid inheriting the firmware-only target/build-std settings from .cargo/config.toml.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
host=$(rustc -vV | sed -n 's/^host: //p')
cd "${TMPDIR:-/tmp}"
exec cargo run --locked --manifest-path "$repo/Cargo.toml" -p espctl --target "$host" -- "$@"
