#!/usr/bin/env bash
set -euo pipefail
hiturbo_app_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ -x "$hiturbo_app_dir/hiturbo-rs" ]]; then
    exec "$hiturbo_app_dir/hiturbo-rs" "$@"
fi
if [[ "$(uname -s)" == "Darwin" ]]; then
    export CMAKE_POLICY_VERSION_MINIMUM="${CMAKE_POLICY_VERSION_MINIMUM:-3.5}"
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
fi
exec cargo run --release --locked --manifest-path "$hiturbo_app_dir/Cargo.toml" -- "$@"
