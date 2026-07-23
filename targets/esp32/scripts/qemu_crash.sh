#!/bin/bash
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR/.."

ITERS=25
ATTACK="none"

while [[ $# -gt 0 ]]; do
  case $1 in
    --iters) ITERS="$2"; shift 2 ;;
    --attack) ATTACK="$2"; shift 2 ;;
    *) echo "Unknown opt: $1"; exit 1 ;;
  esac
done

echo "Running crash campaign (iters: $ITERS, attack: $ATTACK)..."

for i in $(seq 1 $ITERS); do
    echo "Iteration $i..."
    rm -f qemu.log
    ./scripts/qemu_run.sh > qemu.log 2>&1 &
    QEMU_PID=$!
    
    PTY=""
    for try in {1..1000}; do
        PTY=$(grep "char device redirected to" qemu.log | awk '{print $5}' || true)
        if [ -n "$PTY" ]; then
            break
        fi
        sleep 0.1
    done
    
    if [ -z "$PTY" ]; then
        echo "Failed to get PTY"
        cat qemu.log
        kill -9 $QEMU_PID || true
        exit 1
    fi
    # Strip trailing whitespace/commas from PTY
    PTY=$(echo "$PTY" | tr -d ' ,\r\n')
    # Let firmware finish booting before opening serial
    sleep 1
    
    if [ "$i" -eq 1 ]; then
        # On first iteration, setup an initial valid checkpoint and save old image for rollback
        # Retry once on failure — QEMU boot timing can vary in CI
        if ! python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k0 v0" --cmd "seal" --expect "OK" > drive.log 2>&1; then
            echo "First seal attempt failed, retrying after delay..."
            kill -9 $QEMU_PID || true
            wait $QEMU_PID 2>/dev/null || true
            sleep 1
            rm -f qemu.log
            ./scripts/qemu_run.sh > qemu.log 2>&1 &
            QEMU_PID=$!
            PTY=""
            for try in {1..1000}; do
                PTY=$(grep "char device redirected to" qemu.log | awk '{print $5}' || true)
                if [ -n "$PTY" ]; then break; fi
                sleep 0.1
            done
            PTY=$(echo "$PTY" | tr -d ' ,\r\n')
            sleep 1
            if ! python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k0 v0" --cmd "seal" --expect "OK" > drive.log 2>&1; then
                echo "Failed to seal initial checkpoint!"
                cat drive.log
                kill -9 $QEMU_PID || true
                exit 1
            fi
        fi
        cp flash.img flash_old.img
    fi

    # Issue put and commit. If it's a rollback attack, we also force a seal so the counter advances
    if [ "$ATTACK" == "rollback" ]; then
        python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k$i v$i" --cmd "seal" > drive.log 2>&1 &
    else
        python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k$i v$i" --cmd "commit" > drive.log 2>&1 &
    fi
    DRIVE_PID=$!
    
    # Wait for the drive to start sending
    sleep 0.2
    
    RND=$RANDOM
    DELAY=$(echo "scale=3; 0.05 + 0.45 * ($RND / 32767)" | bc)
    sleep $DELAY
    
    kill -9 $QEMU_PID || true
    kill -9 $DRIVE_PID 2>/dev/null || true
    wait $QEMU_PID 2>/dev/null || true
    
    if [ "$ATTACK" == "rollback" ] && [ "$i" -eq $ITERS ]; then
        echo "Injecting rollback attack..."
        cp flash_old.img flash.img
    elif [ "$ATTACK" == "tamper" ] && [ "$i" -eq $ITERS ]; then
        echo "Injecting tamper attack..."
        # Corrupt data in middle
        printf '\x00' | dd of=flash.img bs=1 seek=1000000 count=1 conv=notrunc 2>/dev/null
    fi
    
    # Verify by restarting
    rm -f qemu.log
    ./scripts/qemu_run.sh > qemu.log 2>&1 &
    QEMU_PID=$!
    
    PTY=""
    for try in {1..1000}; do
        PTY=$(grep "char device redirected to" qemu.log | awk '{print $5}' || true)
        if [ -n "$PTY" ]; then
            break
        fi
        sleep 0.1
    done
    PTY=$(echo "$PTY" | tr -d ' ,\r\n')
    # Let firmware finish booting
    sleep 1
    
    EXPECT_STATUS="OK"
    if [ "$ATTACK" == "rollback" ] && [ "$i" -eq $ITERS ]; then
        EXPECT_STATUS="Rollback"
    elif [ "$ATTACK" == "tamper" ] && [ "$i" -eq $ITERS ]; then
        EXPECT_STATUS="Tampered"
    fi
    
    if ! python3 ./scripts/serial_drive.py --port "$PTY" --cmd "selftest" --expect "$EXPECT_STATUS" --cmd "stats" --expect "commits=" > verify.log 2>&1; then
        echo "Verify failed! Expected: $EXPECT_STATUS"
        cat verify.log
        kill -9 $QEMU_PID || true
        exit 1
    fi
    
    kill -9 $QEMU_PID || true
    wait $QEMU_PID 2>/dev/null || true
done

echo "Crash campaign PASSED"
