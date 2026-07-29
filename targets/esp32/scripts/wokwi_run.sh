#!/bin/bash
# Build kv_demo for the ESP32-C3 and run the Wokwi CI scenario against it.
#
# Verifies on an emulated device what the host test suite cannot: that a record
# survives `put` -> `commit` -> `get` on real flash, that a `del` tombstone
# commits from the cold log, and that an epoch seal programs a checkpoint.
#
# Requires WOKWI_CLI_TOKEN (https://wokwi.com/dashboard/ci) and wokwi-cli
# (https://docs.wokwi.com/wokwi-ci/getting-started). The Wokwi CLI opens a
# WebSocket to wokwi.com, so it must run somewhere with direct outbound
# network access.
#
# Usage:
#   export WOKWI_CLI_TOKEN=...
#   ./scripts/wokwi_run.sh
#   ./scripts/wokwi_run.sh --interactive     # drive the shell by hand
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

BIN="kv_demo"
TARGET="riscv32imc-unknown-none-elf"
# The Wokwi board is an esp32-c3-devkitm-1. `counter-flash` keeps the rollback
# counter in its own sector inside the image (BestEffort mode), which is what
# the scenario's `mode` assertion expects.
FEATURES="${FEATURES:-chip-esp32c3,counter-flash}"
TIMEOUT_MS="${TIMEOUT_MS:-120000}"

if [ -z "${WOKWI_CLI_TOKEN:-}" ]; then
    echo "error: WOKWI_CLI_TOKEN is not set." >&2
    echo "       Create a CI token at https://wokwi.com/dashboard/ci" >&2
    exit 2
fi

if ! command -v wokwi-cli >/dev/null 2>&1; then
    echo "error: wokwi-cli not found on PATH." >&2
    echo "       Install: curl -L https://wokwi.com/ci/install.sh | sh" >&2
    exit 2
fi

echo "==> Building $BIN ($FEATURES)"
cargo build --bin "$BIN" --release --features "$FEATURES" --target "$TARGET"

# Absolute path: wokwi.toml carries its own relative `elf =` key, and --elf
# overrides it. Passing an absolute path removes any ambiguity about whether a
# relative path resolves against the cwd, the project dir, or the toml's dir —
# and guards against a stale ELF under a different CARGO_TARGET_DIR.
ELF="$(pwd)/${CARGO_TARGET_DIR:-target}/$TARGET/release/$BIN"
if [ ! -f "$ELF" ]; then
    echo "error: expected ELF at $ELF" >&2
    echo "       (if CARGO_TARGET_DIR is set, it must be an absolute path)" >&2
    exit 1
fi
echo "==> ELF: $ELF ($(wc -c < "$ELF" | tr -d ' ') bytes)"

LOG="${SERIAL_LOG:-wokwi-serial.log}"

# The project directory is the one containing wokwi.toml and diagram.json.
PROJECT_DIR="$(pwd)/wokwi"
LOG_ABS="$(pwd)/$LOG"

if [ "${1:-}" == "--interactive" ]; then
    echo "==> Interactive session (Ctrl-C to exit)"
    exec wokwi-cli --interactive --timeout "$TIMEOUT_MS" \
        --diagram-file "$PROJECT_DIR/diagram.json" \
        --elf "$ELF" \
        "$PROJECT_DIR"
fi

echo "==> Running scenario wokwi/kv_demo.scenario.yaml"
set +e
wokwi-cli \
    --timeout "$TIMEOUT_MS" \
    --scenario "$PROJECT_DIR/kv_demo.scenario.yaml" \
    --diagram-file "$PROJECT_DIR/diagram.json" \
    --elf "$ELF" \
    --serial-log-file "$LOG_ABS" \
    "$PROJECT_DIR"
RC=$?
set -e

echo
if [ "$RC" -eq 0 ]; then
    echo "==> PASS — records committed and read back on emulated flash"
else
    echo "==> FAIL (exit $RC). Serial log: $LOG_ABS"
    echo "--- last 40 lines ---"
    tail -40 "$LOG_ABS" 2>/dev/null || true
    echo
    echo "If commits report 'err commit Io', check the flash region size:"
    echo "  data_base_offset(4096) = 540672 B must fit inside SLATE_FLASH_LEN."
fi
exit "$RC"
