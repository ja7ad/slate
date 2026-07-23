#!/usr/bin/env python3
import sys
import serial
import time
import argparse

global_buf = ""

def expect(ser, text, timeout=60.0, send_nl=False):
    global global_buf
    start = time.time()
    last_nl = start
    while time.time() - start < timeout:
        if text in global_buf:
            # We found the text. Clear up to the end of the text.
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
    if not expect(ser, "slate> ", send_nl=True):
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
