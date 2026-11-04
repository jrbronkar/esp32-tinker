#!/bin/sh
# Select the host target; build-std is enabled only by the esp-* Cargo aliases.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
host=$(rustc -vV | sed -n 's/^host: //p')
cd "$repo"
exec cargo run --locked --manifest-path "$repo/Cargo.toml" -p espctl --target "$host" -- "$@"
