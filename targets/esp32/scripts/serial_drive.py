#!/usr/bin/env python3
import sys
import serial
import time
import argparse

global_buf = ""

def expect(ser, text, timeout=60.0, send_nl=False):
    """Wait until `text` appears in serial output. Returns True on match."""
    global global_buf
    start = time.time()
    last_nl = start
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

        if send_nl and time.time() - last_nl > 1.0:
            ser.write(b'\n')
            ser.flush()
            last_nl = time.time()

        time.sleep(0.01)
    print(f"\n[ERROR] Timeout waiting for: {text}")
    return False

def send(ser, text):
    ser.write((text + "\n").encode('utf-8'))
    ser.flush()

def sync_prompt(ser, timeout=30.0):
    """Synchronize with the firmware REPL by sending a newline and waiting
    for a clean 'slate> ' prompt. Drains any stale data first."""
    global global_buf
    # Drain anything already buffered
    global_buf = ""
    time.sleep(0.2)
    while ser.in_waiting:
        ser.read(ser.in_waiting)
        time.sleep(0.05)

    # Send a single newline to trigger exactly one fresh prompt
    ser.write(b'\n')
    ser.flush()

    # Wait for that prompt
    return expect(ser, "slate> ", timeout=timeout)

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

    # Phase 1: Wait for initial boot prompt (firmware may take a while)
    if not expect(ser, "slate> ", timeout=60, send_nl=True):
        sys.exit(2)

    # Phase 2: Synchronize — drain all ghost prompts from queued newlines,
    # then send one fresh newline and wait for exactly one clean prompt.
    if not sync_prompt(ser):
        print("\n[ERROR] Failed to synchronize prompt")
        sys.exit(2)

    # Phase 3: Execute commands sequentially
    if args.cmd:
        for i, cmd in enumerate(args.cmd):
            send(ser, cmd)
            exp_text = args.expect[i] if (args.expect and i < len(args.expect)) else None
            if exp_text:
                # Non-empty expect: wait for the expected text first
                if not expect(ser, exp_text):
                    sys.exit(3)
            # Always wait for the next prompt (command finished)
            if not expect(ser, "slate> "):
                sys.exit(2)

    print("\n[SUCCESS] Driver finished.")
    sys.exit(0)

if __name__ == "__main__":
    main()
