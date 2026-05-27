#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
bin="$root/.codex/tools/serena-rs/target/release/serena-rs"

if [ ! -x "$bin" ]; then
  bin="$root/.codex/tools/serena-rs/target/debug/serena-rs"
fi

if [ -x "$bin" ]; then
  exec "$bin" "$@"
fi

exec cargo run --quiet --manifest-path "$root/.codex/tools/serena-rs/Cargo.toml" -- "$@"
