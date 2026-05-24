#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

if [[ "${SFTP_PERF_IN_ENV:-0}" != 1 ]] && command -v direnv >/dev/null 2>&1 && [[ -f "$SCRIPT_DIR/.envrc" ]]; then
    exec env SFTP_PERF_IN_ENV=1 direnv exec "$SCRIPT_DIR" "$SCRIPT_PATH" "$@"
fi

exec cargo run --quiet --bin sftp-perf -- "$@"
