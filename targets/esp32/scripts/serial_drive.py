#!/usr/bin/env python3
"""Serial driver for SLATE QEMU crash tests.

Connects to a PTY, waits for the firmware REPL prompt, and sends
commands sequentially, checking expected output between them.

Key design: NEVER sends more than one newline during boot sync.
Multiple queued newlines create ghost prompts in the firmware UART
that corrupt the command/response protocol — they travel through
the PTY kernel buffer and QEMU UART emulation and arrive at the
firmware unpredictably, even after the host-side serial buffer has
been drained.
"""
import sys
import serial
import time
import argparse

global_buf = ""


def expect(ser, text, timeout=60.0):
    """Wait until `text` appears in serial output. Returns True on match."""
    global global_buf
    start = time.time()
    while time.time() - start < timeout:
        if text in global_buf:
            idx = global_buf.find(text)
            global_buf = global_buf[idx + len(text):]
            return True

        if ser.in_waiting:
            chunk = ser.read(ser.in_waiting).decode('utf-8', errors='replace')
            global_buf += chunk
            print(chunk, end='', flush=True)
            if text in global_buf:
                idx = global_buf.find(text)
                global_buf = global_buf[idx + len(text):]
                return True

        time.sleep(0.01)
    print(f"\n[ERROR] Timeout waiting for: {text}")
    return False


def send(ser, text):
    ser.write((text + "\n").encode('utf-8'))
    ser.flush()


def wait_for_ready(ser, timeout=60.0):
    """Wait for the firmware REPL to be ready.

    Strategy:
    1. Wait up to 5s for the boot prompt to appear passively (it's
       likely already in the PTY buffer thanks to the 1s settle in
       qemu_crash.sh).
    2. If not found, send exactly ONE newline and wait for the prompt.
    3. Once a prompt is found, do a long drain (1s) to consume any
       stale data, then send ONE final newline and wait for a clean
       prompt to confirm synchronization.
    """
    global global_buf
    global_buf = ""

    # Phase 1: Try to read the boot prompt passively (no newlines sent)
    start = time.time()
    found = False
    while time.time() - start < 5.0:
        if ser.in_waiting:
            chunk = ser.read(ser.in_waiting).decode('utf-8', errors='replace')
            global_buf += chunk
            print(chunk, end='', flush=True)
        if "slate> " in global_buf:
            found = True
            break
        time.sleep(0.01)

    # Phase 2: If boot prompt not found, send ONE newline
    if not found:
        ser.write(b'\n')
        ser.flush()
        if not expect(ser, "slate> ", timeout=timeout - 5.0):
            return False
    else:
        # Consume the prompt from buffer
        idx = global_buf.find("slate> ")
        global_buf = global_buf[idx + len("slate> "):]

    # Phase 3: Long drain to let any stale data settle, then re-sync
    # with one final newline to confirm the channel is clean.
    time.sleep(1.0)
    while ser.in_waiting:
        chunk = ser.read(ser.in_waiting).decode('utf-8', errors='replace')
        print(chunk, end='', flush=True)
        time.sleep(0.05)
    global_buf = ""

    # Final sync: one newline → one prompt
    ser.write(b'\n')
    ser.flush()
    if not expect(ser, "slate> ", timeout=10.0):
        return False

    return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", required=True, help="Serial port (e.g. /dev/ttys001)")
    parser.add_argument("--cmd", action="append", help="Command to send (e.g. 'put a 1')")
    parser.add_argument("--expect", action="append", help="Expected output after command")
    args = parser.parse_args()

    try:
        ser = serial.Serial(args.port, 115200, timeout=0.1)
    except Exception as e:
        print(f"Failed to open port {args.port}: {e}")
        sys.exit(1)

    # Wait for firmware boot and synchronize the serial channel
    if not wait_for_ready(ser):
        print("\n[ERROR] Failed to synchronize with firmware REPL")
        sys.exit(2)

    # Execute commands sequentially
    if args.cmd:
        for i, cmd in enumerate(args.cmd):
            send(ser, cmd)
            exp_text = args.expect[i] if (args.expect and i < len(args.expect)) else None
            if exp_text:  # skip empty expects (empty string is falsy)
                if not expect(ser, exp_text):
                    sys.exit(3)
            # Always wait for the next prompt (command finished)
            if not expect(ser, "slate> "):
                sys.exit(2)

    print("\n[SUCCESS] Driver finished.")
    sys.exit(0)


if __name__ == "__main__":
    main()
