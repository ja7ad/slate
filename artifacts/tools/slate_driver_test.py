#!/usr/bin/env python3
"""Independent driver test for the SLATE C ABI.

This is deliberately NOT a Rust test. It loads the shipped shared library
through ctypes and drives it exactly as a third-party consumer would --
no cargo, no C compiler, no knowledge of the engine's internals. If this
passes, the artifact in target/release is usable from any language with
an FFI.

What it checks, in order:

  1. The library loads and every documented symbol resolves.
  2. The ABI version the binary reports matches the shipped header.
  3. put -> commit -> get round-trips a value byte-for-byte.
  4. Values survive a full close/reopen cycle (real durability, not cache).
  5. put_durable is durable WITHOUT an explicit commit.
  6. delete removes a key and get reports NOT_FOUND.
  7. The two-call get protocol (size query, then fetch) reports the right
     length and fills the buffer.
  8. A too-small buffer yields BUFFER_TOO_SMALL and does not scribble.
  9. Empty values, long keys and binary (non-UTF-8) payloads.
 10. Invalid arguments (NULL handles, zero-length keys) are refused rather
     than crashing.
 11. A wrong root key on reopen is reported as TAMPERED -- not as success,
     and not as a segfault. This is the security property a caller must be
     able to rely on.
 12. Bulk load of 500 keys, verified by full read-back after reopen.

Usage:
    python3 artifacts/tools/slate_driver_test.py [--lib PATH] [--keep]

Exit status is 0 only if every check passes.
"""

import argparse
import ctypes
import pathlib
import platform
import random
import shutil
import sys
import tempfile

# ---------------------------------------------------------------- constants
# Mirrored from crates/slate-kv-ffi/include/slate.h. The test asserts the
# runtime library agrees with the header rather than trusting these.
SLATE_OK = 0
SLATE_ERR_NOT_FOUND = -1
SLATE_ERR_BUFFER_TOO_SMALL = -2
SLATE_ERR_INVALID_ARG = -3
SLATE_ERR_TAMPERED = -10
SLATE_ERR_ROLLBACK = -11
SLATE_ERR_INTERNAL = -99
SLATE_ERR_IO = -100

ERR_NAME = {
    SLATE_OK: "OK",
    SLATE_ERR_NOT_FOUND: "NOT_FOUND",
    SLATE_ERR_BUFFER_TOO_SMALL: "BUFFER_TOO_SMALL",
    SLATE_ERR_INVALID_ARG: "INVALID_ARG",
    SLATE_ERR_TAMPERED: "TAMPERED",
    SLATE_ERR_ROLLBACK: "ROLLBACK",
    SLATE_ERR_INTERNAL: "INTERNAL",
    SLATE_ERR_IO: "IO",
}

# Per slate.h: SLATE_PROFILE_PI = 0, SLATE_PROFILE_ESP32 = 1.
PROFILE_PI, PROFILE_ESP32 = 0, 1


class SlateOptions(ctypes.Structure):
    """Matches `slate_options` in slate.h."""

    _fields_ = [
        ("capacity_bytes", ctypes.c_uint64),
        ("max_keys", ctypes.c_uint32),
        ("b_commit", ctypes.c_uint32),
        ("theta", ctypes.c_uint32),
        ("profile", ctypes.c_uint8),
    ]


class Results:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.failures = []

    def check(self, name, ok, detail=""):
        if ok:
            self.passed += 1
            print(f"  PASS  {name}")
        else:
            self.failed += 1
            self.failures.append((name, detail))
            print(f"  FAIL  {name}  {detail}")
        return ok

    def eq(self, name, got, want, fmt=str):
        return self.check(name, got == want, f"got {fmt(got)}, want {fmt(want)}")

    def rc(self, name, got, want):
        return self.check(
            name,
            got == want,
            f"got {ERR_NAME.get(got, got)}, want {ERR_NAME.get(want, want)}",
        )


def lib_filename():
    """The shared-library name this platform produces."""
    return {
        "Darwin": "libslate_kv_ffi.dylib",
        "Linux": "libslate_kv_ffi.so",
        "Windows": "slate_kv_ffi.dll",
    }.get(platform.system(), "libslate_kv_ffi.so")


def resolve_lib(arg, repo):
    """Resolve --lib into an actual library file.

    Accepts either the library itself or a directory containing it, since a
    build directory is the natural thing to have on hand -- both
    `target/release` and `target/<triple>/release` are valid places for it.
    Returns (path, note) where `path` is None if nothing was found.
    """
    name = lib_filename()
    if arg is not None:
        p = pathlib.Path(arg)
        if p.is_dir():
            cand = p / name
            return (cand, f"resolved directory to {cand}") if cand.exists() else (None, (
                f"{p} is a directory and contains no {name}\n"
                f"        found: {', '.join(sorted(f.name for f in p.glob('*slate_kv_ffi*'))) or '(nothing)'}"
            ))
        return (p, "") if p.exists() else (None, f"{p} does not exist")

    # No --lib: try the plain release dir, then any per-target dir. A
    # `--target` build lands in target/<triple>/release, which is easy to
    # forget when invoking this.
    candidates = [repo / "target" / "release" / name]
    candidates += sorted((repo / "target").glob(f"*/release/{name}"))
    for c in candidates:
        if c.exists():
            return c, ""
    return None, (
        "no library found in "
        + " or ".join(str(c.parent) for c in candidates[:3])
    )


def header_abi_version(repo):
    """Read the expected ABI version out of the generated header."""
    hdr = repo / "crates" / "slate-kv-ffi" / "include" / "slate.h"
    if not hdr.exists():
        return None
    major = minor = None
    for line in hdr.read_text().splitlines():
        if line.startswith("#define SLATE_ABI_VERSION_MAJOR"):
            major = int(line.split()[-1])
        elif line.startswith("#define SLATE_ABI_VERSION_MINOR"):
            minor = int(line.split()[-1])
    return None if major is None or minor is None else (major, minor)


def bind(lib):
    """Declare every signature.

    ctypes defaults to int-sized arguments, which is wrong for pointers and
    for 64-bit sizes on some ABIs, so this is not optional decoration.
    """
    p_db = ctypes.c_void_p
    u8p = ctypes.POINTER(ctypes.c_uint8)

    lib.slate_abi_version.restype = ctypes.c_uint32
    lib.slate_abi_version.argtypes = []

    lib.slate_open.restype = ctypes.c_int32
    lib.slate_open.argtypes = [
        ctypes.c_char_p,
        u8p,
        ctypes.POINTER(SlateOptions),
        ctypes.POINTER(p_db),
    ]

    lib.slate_put.restype = ctypes.c_int32
    lib.slate_put.argtypes = [p_db, u8p, ctypes.c_size_t, u8p, ctypes.c_size_t]

    lib.slate_put_durable.restype = ctypes.c_int32
    lib.slate_put_durable.argtypes = lib.slate_put.argtypes

    # NOTE the arity: 5 parameters, not 6. `vlen_inout` is in/out — the caller
    # sets it to the buffer capacity and reads back the value's true length.
    # There is no separate capacity argument. Declaring a phantom sixth
    # argument here segfaults on the first call.
    lib.slate_get.restype = ctypes.c_int32
    lib.slate_get.argtypes = [
        p_db,
        u8p,
        ctypes.c_size_t,
        u8p,
        ctypes.POINTER(ctypes.c_size_t),
    ]

    lib.slate_delete.restype = ctypes.c_int32
    lib.slate_delete.argtypes = [p_db, u8p, ctypes.c_size_t]

    for fn in ("slate_commit", "slate_compact", "slate_security_mode", "slate_close"):
        getattr(lib, fn).restype = ctypes.c_int32
        getattr(lib, fn).argtypes = [p_db]

    lib.slate_last_error_message.restype = ctypes.c_int32
    lib.slate_last_error_message.argtypes = [p_db, ctypes.c_char_p, ctypes.c_size_t]
    return lib


def buf(data: bytes):
    """A non-NULL uint8* for `data`, including when it is empty.

    The binding rules forbid NULL for a zero-length slice, so an empty payload
    still needs a valid address.
    """
    if len(data) == 0:
        return (ctypes.c_uint8 * 1)()
    return (ctypes.c_uint8 * len(data))(*data)


class Db:
    """Thin wrapper so the tests read like a consumer's own code."""

    def __init__(self, lib, path, key: bytes, opts: SlateOptions):
        self.lib = lib
        self.handle = ctypes.c_void_p()
        self.rc = lib.slate_open(
            str(path).encode(),
            buf(key),
            ctypes.byref(opts),
            ctypes.byref(self.handle),
        )
        self.closed = self.rc != SLATE_OK

    def put(self, k: bytes, v: bytes, durable=False):
        fn = self.lib.slate_put_durable if durable else self.lib.slate_put
        return fn(self.handle, buf(k), len(k), buf(v), len(v))

    def get(self, k: bytes, cap=4096):
        out = (ctypes.c_uint8 * max(cap, 1))()
        n = ctypes.c_size_t(cap)  # in: capacity; out: true value length
        rc = self.lib.slate_get(self.handle, buf(k), len(k), out, ctypes.byref(n))
        return (rc, bytes(out[: n.value])) if rc == SLATE_OK else (rc, None)

    def get_size(self, k: bytes):
        """Two-call protocol: query the length with a zero-capacity buffer."""
        # NULL buffer + zero capacity: the documented size-query form.
        n = ctypes.c_size_t(0)
        rc = self.lib.slate_get(self.handle, buf(k), len(k), None, ctypes.byref(n))
        return rc, n.value

    def delete(self, k: bytes):
        return self.lib.slate_delete(self.handle, buf(k), len(k))

    def commit(self):
        return self.lib.slate_commit(self.handle)

    def security_mode(self):
        return self.lib.slate_security_mode(self.handle)

    def last_error(self):
        b = ctypes.create_string_buffer(256)
        self.lib.slate_last_error_message(self.handle, b, 256)
        return b.value.decode(errors="replace")

    def close(self):
        if self.closed:
            return SLATE_OK
        rc = self.lib.slate_close(self.handle)
        self.closed = True
        return rc


def run(lib, workdir, r: Results, repo):
    key = bytes([0x42] * 32)
    other_key = bytes([0x43] * 32)

    def opts(b_commit=1):
        # b_commit = 1 by default so a plain `put` is not silently batched:
        # these tests want to distinguish committed from buffered explicitly.
        return SlateOptions(
            capacity_bytes=4 * 1024 * 1024,
            max_keys=8192,
            b_commit=b_commit,
            theta=0,
            profile=PROFILE_PI,
        )

    print("\n[1] ABI and symbol resolution")
    ver = lib.slate_abi_version()
    major, minor = ver >> 16, ver & 0xFFFF
    print(f"      library reports ABI {major}.{minor}")
    expect = header_abi_version(repo)
    if expect:
        r.eq("abi matches shipped header", (major, minor), expect)
    else:
        r.check("abi version is non-zero", ver != 0, f"got {ver}")

    print("\n[2] put / commit / get and durability across reopen")
    dbpath = workdir / "roundtrip.bin"
    db = Db(lib, dbpath, key, opts())
    if not r.rc("open fresh database", db.rc, SLATE_OK):
        return

    r.rc("put", db.put(b"sensor_1", b"23.5 C"), SLATE_OK)
    r.rc("commit", db.commit(), SLATE_OK)
    rc, val = db.get(b"sensor_1")
    r.rc("get after commit", rc, SLATE_OK)
    r.eq("value round-trips byte-for-byte", val, b"23.5 C")

    mode = db.security_mode()
    print(f"      security mode = {mode}")
    r.check("security mode is a valid enum", mode in (0, 1, 2), f"got {mode}")

    r.rc(
        "put_durable without explicit commit",
        db.put(b"k_dur", b"v_dur", durable=True),
        SLATE_OK,
    )
    r.rc("close", db.close(), SLATE_OK)

    db2 = Db(lib, dbpath, key, opts())
    r.rc("reopen with correct key", db2.rc, SLATE_OK)
    rc, val = db2.get(b"sensor_1")
    r.rc("committed value survives reopen", rc, SLATE_OK)
    r.eq("value intact after reopen", val, b"23.5 C")
    rc, val = db2.get(b"k_dur")
    r.rc("put_durable value survives reopen", rc, SLATE_OK)
    r.eq("put_durable value intact", val, b"v_dur")

    print("\n[3] delete")
    r.rc("delete existing key", db2.delete(b"sensor_1"), SLATE_OK)
    r.rc("commit after delete", db2.commit(), SLATE_OK)
    rc, _ = db2.get(b"sensor_1")
    r.rc("get deleted key", rc, SLATE_ERR_NOT_FOUND)
    rc, _ = db2.get(b"never_written")
    r.rc("get absent key", rc, SLATE_ERR_NOT_FOUND)

    print("\n[4] two-call get protocol and buffer handling")
    payload = bytes(range(256)) * 4  # 1024 B, binary, non-UTF-8
    r.rc("put binary payload", db2.put(b"binkey", payload, durable=True), SLATE_OK)
    rc, size = db2.get_size(b"binkey")
    r.check(
        "size query reports length",
        size == len(payload),
        f"got {size}, want {len(payload)} (rc={ERR_NAME.get(rc, rc)})",
    )
    rc, val = db2.get(b"binkey", cap=len(payload))
    r.rc("fetch with exact-size buffer", rc, SLATE_OK)
    r.eq(
        "binary payload round-trips",
        val,
        payload,
        fmt=lambda b: f"{len(b or b'')} B",
    )

    small = (ctypes.c_uint8 * 8)()
    sentinel = bytes(small)
    n = ctypes.c_size_t(8)
    rc = lib.slate_get(db2.handle, buf(b"binkey"), 6, small, ctypes.byref(n))
    r.rc("undersized buffer refused", rc, SLATE_ERR_BUFFER_TOO_SMALL)
    r.eq("undersized buffer left untouched", bytes(small), sentinel, fmt=lambda b: b.hex())

    print("\n[5] empty values and long keys")
    r.rc("put empty value", db2.put(b"empty", b"", durable=True), SLATE_OK)
    rc, val = db2.get(b"empty")
    r.rc("get empty value", rc, SLATE_OK)
    r.eq("empty value is zero-length", val, b"")

    long_key = b"k" * 64
    r.rc("put 64-byte key", db2.put(long_key, b"lk", durable=True), SLATE_OK)
    rc, val = db2.get(long_key)
    r.rc("get 64-byte key", rc, SLATE_OK)
    r.eq("64-byte key value", val, b"lk")

    print("\n[6] invalid arguments")
    null = ctypes.c_void_p(None)
    r.rc("put on NULL handle", lib.slate_put(null, buf(b"k"), 1, buf(b"v"), 1),
         SLATE_ERR_INVALID_ARG)
    r.rc("commit on NULL handle", lib.slate_commit(null), SLATE_ERR_INVALID_ARG)
    r.rc("put with zero-length key", db2.put(b"", b"v"), SLATE_ERR_INVALID_ARG)
    out = (ctypes.c_uint8 * 16)()
    r.rc(
        "get with NULL length pointer",
        lib.slate_get(db2.handle, buf(b"x"), 1, out, None),
        SLATE_ERR_INVALID_ARG,
    )
    r.rc("close", db2.close(), SLATE_OK)

    print("\n[7] security: wrong root key on reopen")
    db3 = Db(lib, dbpath, other_key, opts())
    r.rc("reopen with WRONG key is refused", db3.rc, SLATE_ERR_TAMPERED)
    r.check(
        "refusal is not silent",
        db3.rc != SLATE_OK,
        "a wrong key must never open the database",
    )
    if db3.rc == SLATE_OK:
        db3.close()

    print("\n[8] bulk load and full read-back")
    bulkpath = workdir / "bulk.bin"
    o = opts(b_commit=16)  # amortise the commit, as a real workload would
    dbb = Db(lib, bulkpath, key, o)
    if r.rc("open bulk database", dbb.rc, SLATE_OK):
        rng = random.Random(1234)
        expected = {}
        n_keys = 500
        bad_put = 0
        for i in range(n_keys):
            k = f"key_{i:05d}".encode()
            v = bytes(rng.randrange(256) for _ in range(rng.randrange(1, 64)))
            expected[k] = v
            if dbb.put(k, v) != SLATE_OK:
                bad_put += 1
        r.eq(f"{n_keys} puts accepted", bad_put, 0)
        r.rc("commit bulk", dbb.commit(), SLATE_OK)
        r.rc("close bulk", dbb.close(), SLATE_OK)

        dbb2 = Db(lib, bulkpath, key, o)
        r.rc("reopen bulk", dbb2.rc, SLATE_OK)
        mismatch = missing = 0
        for k, v in expected.items():
            rc, got = dbb2.get(k)
            if rc == SLATE_ERR_NOT_FOUND:
                missing += 1
            elif got != v:
                mismatch += 1
        r.eq("no keys lost after reopen", missing, 0)
        r.eq("no values corrupted after reopen", mismatch, 0)
        dbb2.close()


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--lib",
        type=pathlib.Path,
        help="the SLATE shared library, or a directory containing it "
        "(e.g. target/release or target/<triple>/release)",
    )
    ap.add_argument("--keep", action="store_true", help="keep the scratch databases")
    args = ap.parse_args()

    repo = pathlib.Path(__file__).resolve().parent.parent
    libpath, note = resolve_lib(args.lib, repo)

    print("=" * 68)
    print("SLATE C ABI driver test")
    print("=" * 68)
    print(f"platform : {platform.system()} {platform.machine()}")

    if libpath is None:
        print(f"\nERROR: {note}")
        print(f"\nExpected a file named {lib_filename()}.")
        print("Build it with:  cargo build --release -p slate-kv-ffi")
        print("               (add --target <triple> if you build per-target)")
        return 2

    print(f"library  : {libpath}")
    if note:
        print(f"           ({note})")

    try:
        lib = bind(ctypes.CDLL(str(libpath)))
    except OSError as e:
        print(f"\nERROR: could not load the library: {e}")
        return 2
    except AttributeError as e:
        print(f"\nERROR: a documented symbol is missing: {e}")
        return 2

    workdir = pathlib.Path(tempfile.mkdtemp(prefix="slate_driver_"))
    r = Results()
    try:
        run(lib, workdir, r, repo)
    finally:
        if args.keep:
            print(f"\nscratch databases kept in {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)

    print("\n" + "=" * 68)
    total = r.passed + r.failed
    if r.failed == 0:
        print(f"ALL {total} CHECKS PASSED - the driver is working")
    else:
        print(f"{r.failed} of {total} CHECKS FAILED")
        for name, detail in r.failures:
            print(f"  - {name}: {detail}")
    print("=" * 68)
    return 0 if r.failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())