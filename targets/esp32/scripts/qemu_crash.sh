#!/bin/bash
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

ITERS=10
ATTACK="none"

while [[ $# -gt 0 ]]; do
  case $1 in
    --iters) ITERS="$2"; shift 2 ;;
    --attack) ATTACK="$2"; shift 2 ;;
    *) echo "Unknown opt: $1"; exit 1 ;;
  esac
done

echo "Running crash campaign (iters: $ITERS, attack: $ATTACK)..."
# Stub implementation

for i in $(seq 1 $ITERS); do
    echo "Iteration $i..."
    # 1. run qemu_run.sh
    # 2. extract pty
    # 3. python serial_drive.py --port PTY --cmd "put k$i v$i" --cmd "commit"
    # 4. sleep random & kill
    # 5. verify
done

echo "Crash campaign PASSED"
