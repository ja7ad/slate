#!/bin/bash
# Run the kv_demo scenario against every ESP32 board Wokwi emulates, and print
# a pass/fail table with the evidence line for each.
#
# This is the only way to verify the multi-chip port actually BOOTS: a
# successful cross-compile says nothing about whether esp-storage's flash
# backend works on that silicon, and the C3 defects found earlier (read-path,
# cold-log head collision, alignment) were all invisible to the host suite.
#
# Wokwi is reachable only from a machine with direct outbound network access --
# the CLI opens a WebSocket to wokwi.com and its bundled `ws` library ignores
# HTTPS_PROXY.
#
# Usage:
#   export WOKWI_CLI_TOKEN=...          # https://wokwi.com/dashboard/ci
#   ./scripts/wokwi_matrix.sh           # every board with a diagram
#   ./scripts/wokwi_matrix.sh esp32c6   # a subset
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

WOKWI="${WOKWI:-$HOME/.wokwi/bin/wokwi-cli}"
ELFDIR="${ELFDIR:-build_logs}"
DIAGDIR="${DIAGDIR:-wokwi}"
SCENARIO="${SCENARIO:-$DIAGDIR/kv_demo.scenario.yaml}"
TIMEOUT_MS="${TIMEOUT_MS:-120000}"
LOGDIR="${LOGDIR:-wokwi_logs}"
mkdir -p "$LOGDIR"

if [ -z "${WOKWI_CLI_TOKEN:-}" ]; then
  echo "error: WOKWI_CLI_TOKEN is not set (https://wokwi.com/dashboard/ci)" >&2
  exit 2
fi

# Only chips Wokwi actually emulates. esp32-c2, c5 and c61 have no Wokwi board,
# so they are build-verified only -- reported here as SKIP rather than silently
# omitted, so the table matches the chip list in Cargo.toml.
BOARDS=(esp32 esp32c3 esp32c6 esp32h2 esp32s2 esp32s3)
NO_BOARD=(esp32c2 esp32c5 esp32c61)

SELECTED=("$@")
want() {
  [ ${#SELECTED[@]} -eq 0 ] && return 0
  for s in "${SELECTED[@]}"; do [ "$s" == "$1" ] && return 0; done
  return 1
}

printf '%-10s %-10s %-8s %s\n' CHIP RESULT EXIT EVIDENCE
printf '%s\n' "----------------------------------------------------------------------"

rc_overall=0
for chip in "${BOARDS[@]}"; do
  want "$chip" || continue

  elf="$ELFDIR/kv_demo-${chip}.elf"
  diag="$DIAGDIR/diagram-${chip}.json"
  log="$LOGDIR/${chip}.serial.log"

  if [ ! -f "$elf" ]; then
    printf '%-10s %-10s %-8s %s\n' "$chip" "SKIP" "-" "no ELF (run build_matrix.sh)"
    continue
  fi
  if [ ! -f "$diag" ]; then
    printf '%-10s %-10s %-8s %s\n' "$chip" "SKIP" "-" "no diagram-${chip}.json"
    continue
  fi

  # Absolute paths: wokwi-cli resolves --diagram-file and --scenario relative to
  # the PROJECT DIR, not the cwd, so relative paths silently miss.
  # WOKWI_CLI_TOKEN must reach the CLI's own environment; exporting it in a
  # parent shell that then pipes this script through `tee` is not enough on all
  # shells, so it is passed explicitly.
  WOKWI_CLI_TOKEN="$WOKWI_CLI_TOKEN" "$WOKWI" \
    --timeout "$TIMEOUT_MS" \
    --scenario "$(pwd)/$SCENARIO" \
    --diagram-file "$(pwd)/$diag" \
    --elf "$(pwd)/$elf" \
    --serial-log-file "$(pwd)/$log" \
    --timeout-exit-code 42 \
    "$(pwd)/$DIAGDIR" >"$LOGDIR/${chip}.cli.log" 2>&1
  rc=$?

  if [ $rc -eq 0 ]; then
    # Quote the line that proves a record round-tripped through real flash.
    ev=$(grep -m1 -E 'ack [0-9]+' "$log" 2>/dev/null | tr -d '\r' | cut -c1-40)
    printf '%-10s %-10s %-8s %s\n' "$chip" "PASS" "$rc" "${ev:-scenario completed}"
  else
    if [ $rc -eq 42 ]; then
      why="timeout; last: $(tail -1 "$log" 2>/dev/null | tr -d '\r' | cut -c1-32)"
    else
      why=$(grep -m1 -iE 'error|panic' "$LOGDIR/${chip}.cli.log" 2>/dev/null | cut -c1-48)
    fi
    printf '%-10s %-10s %-8s %s\n' "$chip" "FAIL" "$rc" "${why:-see $LOGDIR/${chip}.cli.log}"
    rc_overall=1
  fi
done

for chip in "${NO_BOARD[@]}"; do
  want "$chip" || continue
  printf '%-10s %-10s %-8s %s\n' "$chip" "SKIP" "-" "no Wokwi board; build-verified only"
done

echo
echo "serial logs in $LOGDIR/"
exit "$rc_overall"
