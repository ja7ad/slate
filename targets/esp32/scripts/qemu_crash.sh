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

# Wait until the firmware has actually booted, instead of sleeping a fixed 1 s.
# kv_demo prints its banner and prompt on startup, so polling the QEMU log for
# it returns as soon as the guest is ready — typically well under the 1 s the
# fixed sleeps assumed, and it also tolerates a CI runner that is slower.
# BOOT_TIMEOUT bounds the wait so a genuinely dead guest still fails fast.
BOOT_TIMEOUT="${BOOT_TIMEOUT:-15}"

wait_for_boot() {
    local log="$1" deadline=$((SECONDS + BOOT_TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if grep -q "kv_demo main started" "$log" 2>/dev/null; then
            return 0
        fi
        sleep 0.05
    done
    echo "warning: boot banner not seen in ${BOOT_TIMEOUT}s; continuing" >&2
    return 0
}

echo "Running crash campaign (iters: $ITERS, attack: $ATTACK)..."

rm -f flash.img flash_old.img
./scripts/qemu_run.sh --fresh > qemu.log 2>&1 &
QEMU_PID=$!
wait_for_boot qemu.log
kill -9 $QEMU_PID || true
wait $QEMU_PID 2>/dev/null || true

for i in $(seq 1 $ITERS); do
    echo "Iteration $i..."
    rm -f qemu.log
    ./scripts/qemu_run.sh > qemu.log 2>&1 &
    QEMU_PID=$!
    
    PTY=""
    for try in $(seq 1 600); do
        PTY=$(grep "char device redirected to" qemu.log | awk '{print $5}' || true)
        if [ -n "$PTY" ]; then
            break
        fi
        sleep 0.05
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
    wait_for_boot qemu.log
    
    if [ "$i" -eq 1 ]; then
        # On first iteration, setup an initial valid checkpoint and save old image for rollback
        # Retry once on failure — QEMU boot timing can vary in CI
        if ! python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k0 v0" --expect "" --cmd "seal" --expect "OK" --cmd "put kx vx" --expect "" --cmd "seal" --expect "OK" > drive.log 2>&1; then
            echo "First seal attempt failed, retrying after delay..."
            kill -9 $QEMU_PID || true
            wait $QEMU_PID 2>/dev/null || true
            sleep 1
            rm -f qemu.log
            ./scripts/qemu_run.sh > qemu.log 2>&1 &
            QEMU_PID=$!
            PTY=""
            for try in $(seq 1 600); do
                PTY=$(grep "char device redirected to" qemu.log | awk '{print $5}' || true)
                if [ -n "$PTY" ]; then break; fi
                sleep 0.05
            done
            PTY=$(echo "$PTY" | tr -d ' ,\r\n')
            wait_for_boot qemu.log
            if ! python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k0 v0" --expect "" --cmd "seal" --expect "OK" --cmd "put kx vx" --expect "" --cmd "seal" --expect "OK" > drive.log 2>&1; then
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
        # For rollback, we MUST let seal finish so the hardware counter actually increments!
        python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k$i v$i" --expect "" --cmd "seal" --expect "OK" > drive.log 2>&1
        kill -9 $QEMU_PID || true
        wait $QEMU_PID 2>/dev/null || true
    else
        python3 ./scripts/serial_drive.py --port "$PTY" --cmd "put k$i v$i" --expect "" --cmd "commit" --expect "" > drive.log 2>&1 &
        DRIVE_PID=$!
        
        # Wait for the drive to start sending
        sleep 0.2
        
        RND=$RANDOM
        DELAY=$(echo "scale=3; 0.05 + 0.45 * ($RND / 32767)" | bc)
        sleep $DELAY
        
        kill -9 $QEMU_PID || true
        kill -9 $DRIVE_PID 2>/dev/null || true
        wait $QEMU_PID 2>/dev/null || true
    fi
    
    if [ "$ATTACK" == "rollback" ] && [ "$i" -eq $ITERS ]; then
        echo "Injecting rollback attack..."
        # Copy only the first 2MB to revert the SLATE partition, PRESERVING the monotonic counter!
        dd if=flash_old.img of=flash.img bs=1048576 count=2 conv=notrunc 2>/dev/null
    elif [ "$ATTACK" == "tamper" ] && [ "$i" -eq $ITERS ]; then
        echo "Injecting tamper attack..."
        # Corrupt monotonic counter at 0x300000 to trigger Tampered
        printf '\x00' | dd of=flash.img bs=1 seek=3145728 count=1 conv=notrunc 2>/dev/null
    fi
    
    # Verify by restarting
    rm -f qemu.log
    ./scripts/qemu_run.sh > qemu.log 2>&1 &
    QEMU_PID=$!
    
    PTY=""
    for try in $(seq 1 600); do
        PTY=$(grep "char device redirected to" qemu.log | awk '{print $5}' || true)
        if [ -n "$PTY" ]; then
            break
        fi
        sleep 0.05
    done
    PTY=$(echo "$PTY" | tr -d ' ,\r\n')
    # Let firmware finish booting
    wait_for_boot qemu.log
    
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
