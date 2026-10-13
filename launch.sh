#!/usr/bin/env bash
set -euo pipefail
hiturbo_app_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ -x "$hiturbo_app_dir/hiturbo-rs" ]]; then
    exec "$hiturbo_app_dir/hiturbo-rs" "$@"
fi
exec cargo run --release --locked --manifest-path "$hiturbo_app_dir/Cargo.toml" -- "$@"
