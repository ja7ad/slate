#!/bin/bash
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

BIN="kv_demo"
# `metrics` is load-bearing, not optional: qemu_crash.sh's verify step asserts on
# "commits=" from the `stats` command, and that whole block is behind
# #[cfg(feature = "metrics")] in kv_demo.rs. Building without it makes `stats`
# print "metrics: DISABLED" instead, so every verify times out waiting for a
# string the firmware can never emit.
FEATURES="${FEATURES:-chip-esp32c3,counter-efuse,metrics}"

cargo build --bin $BIN --release --no-default-features --features "$FEATURES" --target riscv32imc-unknown-none-elf

IMAGE="flash.img"
if [ "${1:-}" == "--fresh" ] || [ ! -f "$IMAGE" ]; then
    echo "Creating fresh $IMAGE"
    # Needs espflash installed: cargo install espflash
    espflash save-image --ignore-app-descriptor --merge --chip esp32c3 target/riscv32imc-unknown-none-elf/release/$BIN $IMAGE
    # Pad to exactly 4MB with 0xFF (NOR flash erased state).
    # truncate/dd-zero fills with 0x00 which is wrong — EspFlash::program()
    # rejects writes to non-0xFF pages with ProgramWithoutErase.
    CURRENT_SIZE=$(wc -c < "$IMAGE" | tr -d ' ')
    TARGET_SIZE=4194304
    if [ "$CURRENT_SIZE" -lt "$TARGET_SIZE" ]; then
        PAD_SIZE=$((TARGET_SIZE - CURRENT_SIZE))
        python3 -c "import sys; sys.stdout.buffer.write(b'\\xff' * $PAD_SIZE)" >> "$IMAGE"
    fi
fi

qemu-system-riscv32 -nographic \
    -machine esp32c3 \
    -drive file=$IMAGE,if=mtd,format=raw \
    -serial pty \
    -monitor none

