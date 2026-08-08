#!/bin/bash
# Build kv_demo for every ESP32 chip SLATE supports and print a pass/fail table.
#
# The chip list is the INTERSECTION of esp-hal 1.1.1 and esp-storage 0.9.0:
# SLATE needs the flash backend, so a chip the HAL alone supports is not enough.
# Three Rust targets are involved and each chip must be paired with the right
# one -- passing `--features chip-esp32s3` with a RISC-V target fails inside
# esp-hal's build script with a message about the host environment, which is a
# confusing way to learn you picked the wrong triple.
#
# The three Xtensa chips need the espup toolchain fork; they are SKIPPED with a
# reason rather than reported as failures when it is absent, so the table
# distinguishes "does not build" from "cannot be built here".
#
# Usage:
#   ./scripts/build_matrix.sh                # all chips
#   ./scripts/build_matrix.sh esp32c6 esp32  # a subset
#   FEATURES_EXTRA=metrics ./scripts/build_matrix.sh
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

BIN="${BIN:-kv_demo}"
# `counter-flash` keeps the rollback counter in its own sector (BestEffort
# mode); it is the configuration the Wokwi scenarios assert against.
FEATURES_BASE="${FEATURES_BASE:-counter-flash}"
FEATURES_EXTRA="${FEATURES_EXTRA:-}"

# chip:target:arch
CHIPS=(
  "esp32c2:riscv32imc-unknown-none-elf:riscv"
  "esp32c3:riscv32imc-unknown-none-elf:riscv"
  "esp32c5:riscv32imac-unknown-none-elf:riscv"
  "esp32c6:riscv32imac-unknown-none-elf:riscv"
  "esp32c61:riscv32imac-unknown-none-elf:riscv"
  "esp32h2:riscv32imac-unknown-none-elf:riscv"
  "esp32:xtensa-esp32-none-elf:xtensa"
  "esp32s2:xtensa-esp32s2-none-elf:xtensa"
  "esp32s3:xtensa-esp32s3-none-elf:xtensa"
)

# The Xtensa fork needs its export file sourced (it sets LIBCLANG_PATH and puts
# the xtensa-esp-elf GCC on PATH). espup writes it to $HOME by default; allow an
# override because ~/.espup is not always writable.
ESP_EXPORT="${ESP_EXPORT:-$HOME/export-esp.sh}"
if [ -f "$ESP_EXPORT" ]; then
  # shellcheck disable=SC1090
  . "$ESP_EXPORT"
fi

# Is the Xtensa fork installed? `rustup run esp rustc` is the real test: the
# toolchain directory can exist with only GCC/LLVM in it and no rustc, which is
# exactly what a partially-failed espup install leaves behind (the LLVM step can
# fail on an unwritable ~/.espup while GCC is already unpacked).
XTENSA_OK=0
if rustup run esp rustc --version >/dev/null 2>&1; then
  XTENSA_OK=1
fi

SELECTED=("$@")
LOGDIR="${LOGDIR:-build_logs}"
mkdir -p "$LOGDIR"

printf '%-10s %-30s %-8s %-10s %s\n' CHIP TARGET ARCH RESULT DETAIL
printf '%s\n' "-------------------------------------------------------------------------------"

rc_overall=0
for entry in "${CHIPS[@]}"; do
  IFS=: read -r chip target arch <<< "$entry"

  if [ ${#SELECTED[@]} -gt 0 ]; then
    match=0
    for s in "${SELECTED[@]}"; do [ "$s" == "$chip" ] && match=1; done
    [ $match -eq 0 ] && continue
  fi

  feats="chip-${chip},${FEATURES_BASE}"
  [ -n "$FEATURES_EXTRA" ] && feats="${feats},${FEATURES_EXTRA}"
  log="$LOGDIR/${chip}.log"

  if [ "$arch" == "xtensa" ] && [ "$XTENSA_OK" -eq 0 ]; then
    printf '%-10s %-30s %-8s %-10s %s\n' "$chip" "$target" "$arch" "SKIP" \
      "no esp toolchain (espup install)"
    continue
  fi

  # The Xtensa fork ships rust-src but NO precompiled `core` for the
  # xtensa-*-none-elf targets (its rustlib holds only the host triple), so a
  # plain build dies with `can't find crate for core` on the first no_std
  # dependency. `-Z build-std=core,alloc` compiles core from the bundled source;
  # it needs the nightly-only flag, which the fork is (1.95.0-nightly).
  extra=()
  if [ "$arch" == "xtensa" ]; then
    cargo_cmd=(rustup run esp cargo)
    extra=(-Z build-std=core,alloc)
  else
    cargo_cmd=(cargo)
  fi

  # `${extra[@]}` on an EMPTY array is an unbound-variable error under `set -u`
  # in bash < 4.4 (macOS ships 3.2), so the RISC-V path -- which needs no extra
  # flags -- died before running cargo at all. `${extra[@]+"${extra[@]}"}`
  # expands to nothing when the array is empty and to the elements otherwise.
  if "${cargo_cmd[@]}" build --bin "$BIN" --release \
       --features "$feats" --target "$target" ${extra[@]+"${extra[@]}"} >"$log" 2>&1; then
    elf="target/$target/release/$BIN"
    size=$(wc -c < "$elf" 2>/dev/null | tr -d ' ')
    # Keep the per-chip ELF: the next chip's build overwrites the path.
    cp "$elf" "$LOGDIR/${BIN}-${chip}.elf" 2>/dev/null
    printf '%-10s %-30s %-8s %-10s %s\n' "$chip" "$target" "$arch" "PASS" "${size} B"
  else
    detail=$(grep -m1 -E '^error(\[|:)' "$log" | cut -c1-60)
    printf '%-10s %-30s %-8s %-10s %s\n' "$chip" "$target" "$arch" "FAIL" "${detail:-see $log}"
    rc_overall=1
  fi
done

echo
echo "logs and per-chip ELFs in $LOGDIR/"
exit "$rc_overall"
