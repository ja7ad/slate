#!/bin/bash
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

BIN="kv_demo"
FEATURES="chip-esp32c3,counter-flash"

cargo build --bin $BIN --release --no-default-features --features "$FEATURES" --target riscv32imc-unknown-none-elf

IMAGE="flash.img"
if [ "${1:-}" == "--fresh" ] || [ ! -f "$IMAGE" ]; then
    echo "Creating fresh $IMAGE"
    # Needs espflash installed: cargo install espflash
    espflash save-image --ignore-app-descriptor --merge --chip esp32c3 target/riscv32imc-unknown-none-elf/release/$BIN $IMAGE
fi

qemu-system-riscv32 -nographic \
    -machine esp32c3 \
    -drive file=$IMAGE,if=mtd,format=raw \
    -serial pty
