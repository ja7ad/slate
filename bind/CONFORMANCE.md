# SLATE Language Bindings Conformance Suite

Every binding under `bind/` must pass this suite. Implement each case in the
host language's own test framework, naming the test function after the case ID
(`TestC1`, `test_c1`, `c1Test` — whatever your language's convention is, as
long as the ID appears in the name). Uniform IDs make results comparable across
languages and make a failure report actionable in one line: "C8 fails on
Python" says precisely what is broken.

## How to read this suite

The cases are grouped by what they protect. **C1–C13 are behavioural** — they
check that the binding translates the C ABI faithfully. **C14–C18 are
environmental** — they check that the binding is *linked and loaded* correctly,
and every one of them exists because a real failure got past the behavioural
cases.

That distinction matters. A binding can pass C1–C13 on your laptop and still
crash in CI, because the behavioural cases assume the library loaded correctly
in the first place. C14–C18 test that assumption.

## Before you start: run the driver test

Prove the artifact is sound before you debug your binding:

```sh
python3 artifacts/tools/slate_driver_test.py
```

This drives the shared library through Python `ctypes` — no Rust toolchain, no
C compiler, no binding code involved. 44 checks. If it passes and your binding
fails, the fault is in the binding; if it fails, stop and fix the library
first. This is the fastest way to cut the search space in half.

| ID  | Scenario                                                                         | Expected Result                                                                          |
|-----|----------------------------------------------------------------------------------|------------------------------------------------------------------------------------------|
| C1  | Open a fresh database directory with a 32-byte key, `ProfilePi`, and `BCommit=1` | Successfully returns a valid, non-null database handle                                   |
| C2  | Perform a durable write (`PutDurable`) followed by a read (`Get`)                | The retrieved value bytes match the written bytes exactly                                |
| C3  | Attempt to read (`Get`) an absent key                                            | Returns a key-not-found error mapped idiomatically to the host language                  |
| C4  | Read into a 0-capacity buffer, then read into a buffer of the exact size         | Returns buffer-too-small error along with required byte length; subsequent read succeeds |
| C5  | Delete a key (`Delete`) and attempt to read it (`Get`)                           | Returns key-not-found error                                                              |
| C6  | Write to buffer (`Put`), commit (`Commit`), close, reopen, and read (`Get`)      | The written value survives reopening and is retrieved successfully                       |
| C7  | Write and read a zero-length value                                               | Successfully round-trips an empty value without null pointer errors                      |
| C8  | Close, modify bytes in `counter.bin` to simulate tampering, and reopen           | Returns a distinct tamper-detected error, distinguishable from generic I/O errors        |
| C9  | Query security mode on file-backed storage (`SecurityMode`)                      | Returns best-effort rollback protection mode (`SecurityModeBestEffortRollback`)          |
| C10 | Close the handle twice; attempt any operation after closing                      | Returns a typed closed-database error without crashing or memory corruption              |
| C11 | Inspect error message after a failed operation or failed database opening        | Returns a non-empty, descriptive error string explaining the failure                     |
| C12 | Pass an invalid key length (not 32 bytes) when opening                           | Rejected cleanly by the binding before invoking the native C interface                   |
| C13 | Check dynamic ABI version against binding expectation                            | Major version matches expected ABI version                                               |

### Environmental cases (C14–C18)

These check that the binding is correctly linked and loaded. Each one is here
because it caught a real failure that C1–C13 missed.

| ID | Scenario | Expected Result |
|---|---|---|
| C14 | Open and close a database on the runtime's **smallest** unit of concurrency (goroutine, coroutine, fiber, green thread) rather than the main thread | Completes without a crash. `slate_open` needs ≥52 KiB of stack; a runtime with small fixed coroutine stacks faults here and nowhere else |
| C15 | Fail an open (e.g. oversized capacity) on one thread, then read the message via `slate_last_error_message(NULL, …)` on a **different** thread | Returns the same non-empty message. A thread-local error slot returns an empty string and fails this case |
| C16 | Call `slate_get` with a **zero-capacity** buffer, then with a buffer one byte **short**, then exact-size | Size query reports the true length; the short buffer returns `BUFFER_TOO_SMALL` and leaves the caller's bytes untouched; the exact-size read succeeds |
| C17 | Round-trip a **binary, non-UTF-8** value (e.g. all 256 byte values) and a **maximum-length key** | Both round-trip byte-for-byte. Catches bindings that treat values as text or truncate at a NUL |
| C18 | Open with `SLATE_PROFILE_PI`, then assert the constant's value is `0` (and `SLATE_PROFILE_ESP32` is `1`) | Matches `slate.h`. Catches a binding that hardcoded the constants inverted — which does not error, it silently selects the wrong flash geometry |

## Notes on the cases that matter most

**C8 — tamper identity.** The critical security case. It proves the binding
preserves the *identity* of a tamper error rather than flattening it into a
generic I/O exception. A caller must be able to distinguish "the storage was
modified" from "the disk was busy", because the correct response differs
absolutely: never retry a tamper error. Assert the distinct error type, not
merely that some error occurred.

**C11 — error text, not just error presence.** Assert on the *content* of the
message (e.g. that a capacity failure mentions `capacity_bytes`). Asserting
only that the string is non-empty passes on the binding's own format string
without ever exercising `slate_last_error_message`, which is the only channel
for a failed open's reason.

**C14 — the small-stack case.** Run this on whatever the host runtime uses for
lightweight concurrency, not the main thread. A test framework that dispatches
each test onto a coroutine will hit this on the *first* case that touches the
engine, producing a bare SIGSEGV with no stack trace and no hint that stack
depth is the issue.

**C15 — cross-thread error reporting.** Nothing in the C ABI promises thread
affinity, so a caller may legitimately open on a worker and read the error on
the caller's thread. This case fails against a thread-local error slot, which
is why the FFI uses a process-wide mutex.

## Environment matrix

Behavioural correctness is not enough — a binding must also declare and test
the environments it supports:

| Axis | What to verify |
|---|---|
| **libc** | The static library and host runtime must share a libc. Mixing glibc and musl links cleanly and then faults at runtime. Build the archive for the host's target (`--target x86_64-unknown-linux-musl` for a musl host) |
| **Linkage** | Static (`.a`) and shared (`.so`/`.dylib`/`.dll`) exercise different startup paths — test whichever you ship |
| **Stack** | See C14. Document the minimum, and raise it in your test harness if the runtime's default is below 52 KiB |
| **ABI version** | Check `slate_abi_version()` against the header's `SLATE_ABI_VERSION_MAJOR` at open time, not at build time (C13) |

A binding's README should state which combinations it is tested against. "Works
on Linux" is not a claim you can support without saying which libc.
