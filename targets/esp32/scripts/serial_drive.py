#!/usr/bin/env python3
import sys
import serial
import time
import argparse

def expect(ser, text, timeout=5.0):
    start = time.time()
    buf = ""
    while time.time() - start < timeout:
        if ser.in_waiting:
            chunk = ser.read(ser.in_waiting).decode('utf-8', errors='replace')
            buf += chunk
            print(chunk, end='', flush=True)
            if text in buf:
                return True
        time.sleep(0.01)
    print(f"\n[ERROR] Timeout waiting for: {text}")
    return False

def send(ser, text):
    ser.write((text + "\n").encode('utf-8'))
    ser.flush()

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

    # Wait for prompt
    if not expect(ser, "slate> "):
        sys.exit(2)

    if args.cmd:
        for i, cmd in enumerate(args.cmd):
            send(ser, cmd)
            if args.expect and i < len(args.expect):
                if not expect(ser, args.expect[i]):
                    sys.exit(3)
            # Wait for next prompt
            if not expect(ser, "slate> "):
                sys.exit(2)

    print("\n[SUCCESS] Driver finished.")
    sys.exit(0)

if __name__ == "__main__":
    main()
