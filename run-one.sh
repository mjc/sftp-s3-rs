#!/usr/bin/env bash
# Single upload+download against an already-running server.
# Hyperfine measures the wall time; no timing output here.
# Usage: run-one.sh [--client openssh|rust] [--ciphers c1,c2] <port> <testfile_path> [client_source_dir]
# client_source_dir: where client reads file from (default: dirname of testfile_path)

CLIENT="openssh"
CIPHERS="${SFTP_BENCH_CIPHERS:-}"
RUST_CHUNK_SIZE="${SFTP_BENCH_CHUNK_SIZE:-64KiB}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
while [[ "${1:-}" == --* ]]; do
    case "$1" in
        --client)
            if [[ $# -lt 2 || -z "${2:-}" || "${2:-}" == --* ]]; then
                echo "missing value for --client" >&2
                echo "Usage: $0 [--client openssh|rust] [--ciphers c1,c2] <port> <testfile_path> [client_source_dir]" >&2
                exit 2
            fi
            CLIENT=$2
            shift 2
            ;;
        --ciphers)
            if [[ $# -lt 2 || -z "${2:-}" || "${2:-}" == --* ]]; then
                echo "missing value for --ciphers" >&2
                echo "Usage: $0 [--client openssh|rust] [--ciphers c1,c2] <port> <testfile_path> [client_source_dir]" >&2
                exit 2
            fi
            CIPHERS=$2
            shift 2
            ;;
        --)
            shift
            break
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

[[ $# -ge 2 ]] || {
    echo "usage: $0 [--client openssh|rust] [--ciphers c1,c2] <port> <testfile_path> [client_source_dir]" >&2
    exit 2
}

PORT=$1
TESTFILE=$2
CLIENT_SOURCE_DIR=${3:-.}  # default to current dir
USER="benchmark"
PASS="benchmark"
DLFILE="/tmp/sftp-dl-${PORT}.bin"
SFTPOPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o Compression=no)
size_bytes=$(wc -c < "$TESTFILE" | tr -d '[:space:]')

case "$CLIENT" in
    openssh|rust) ;;
    *)
        echo "unknown benchmark client '$CLIENT' (expected openssh or rust)" >&2
        exit 2
        ;;
esac

openssh_ciphers() {
    local raw=$1
    local out=()
    local cipher
    IFS=',' read -ra _cipher_parts <<< "$raw"
    for cipher in "${_cipher_parts[@]}"; do
        case "$cipher" in
            aes256-gcm) out+=("aes256-gcm@openssh.com") ;;
            chacha20-poly1305) out+=("chacha20-poly1305@openssh.com") ;;
            *) out+=("$cipher") ;;
        esac
    done
    local IFS=,
    echo "${out[*]}"
}

if [[ "$CLIENT" == "rust" ]]; then
    RUST_CLIENT=${SFTP_BENCH_CLIENT_BIN:-"$SCRIPT_DIR/target/release/sftp-bench-client"}
    [[ -x "$RUST_CLIENT" ]] || {
        echo "Rust benchmark client not found at $RUST_CLIENT; set SFTP_BENCH_CLIENT_BIN or build it first" >&2
        exit 1
    }

    RUST_ARGS=(
        --host 127.0.0.1 \
        --port "$PORT" \
        --user "$USER" \
        --password "$PASS" \
        --operation roundtrip \
        --size "${size_bytes}B" \
        --iterations 1 \
        --chunk-size "$RUST_CHUNK_SIZE" \
        --insecure \
    )
    if [[ -n "$CIPHERS" ]]; then
        RUST_ARGS+=(--ciphers "$CIPHERS")
    fi

    "$RUST_CLIENT" "${RUST_ARGS[@]}" >/dev/null 2>&1
    exit $?
fi

if [[ -n "${SFTP_IDENTITY_FILE:-}" ]]; then
    SFTP_CMD=(sftp -o BatchMode=yes -o IdentityFile="$SFTP_IDENTITY_FILE")
else
    SFTP_CMD=(sshpass -p "$PASS" sftp -o BatchMode=no -o PreferredAuthentications=password -o PubkeyAuthentication=no)
fi
if [[ -n "$CIPHERS" ]]; then
    SFTP_CMD+=(-c "$(openssh_ciphers "$CIPHERS")")
fi

# Use file from client_source_dir instead of original testfile location
FILENAME=$(basename "$TESTFILE")
CLIENT_FILE="$CLIENT_SOURCE_DIR/$FILENAME"

# Create batch files to avoid rekey issues with heredoc
BATCH_UP="/tmp/sftp_batch_up_$$.txt"
BATCH_DL="/tmp/sftp_batch_dl_$$.txt"

echo "put $CLIENT_FILE" > "$BATCH_UP"
echo "quit" >> "$BATCH_UP"

echo "get $FILENAME $DLFILE" > "$BATCH_DL"
echo "quit" >> "$BATCH_DL"

if [[ -n "${SFTP_IDENTITY_FILE:-}" ]]; then
    "${SFTP_CMD[@]}" -b "$BATCH_UP" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" >/dev/null 2>&1
else
    "${SFTP_CMD[@]}" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" < "$BATCH_UP" >/dev/null 2>&1
fi
[[ $? -eq 0 ]] || { rm -f "$BATCH_UP" "$BATCH_DL"; exit 1; }

if [[ -n "${SFTP_IDENTITY_FILE:-}" ]]; then
    "${SFTP_CMD[@]}" -b "$BATCH_DL" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" >/dev/null 2>&1
else
    "${SFTP_CMD[@]}" "${SFTPOPTS[@]}" -P "$PORT" "$USER@localhost" < "$BATCH_DL" >/dev/null 2>&1
fi
[[ $? -eq 0 ]] || { rm -f "$BATCH_UP" "$BATCH_DL"; exit 1; }

rm -f "$DLFILE" "$BATCH_UP" "$BATCH_DL"
