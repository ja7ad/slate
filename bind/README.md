# SLATE Language Bindings (`bind/`)

This directory is the home for non-Rust language integrations of **SLATE**. All bindings sit directly on the stable C ABI exposed by `slate-kv-ffi`.

## What is a Binding?

A SLATE language binding is a lightweight, idiomatic translation layer over the native C library (`libslate_kv_ffi`). It provides native language types, memory management, and error handling without adding caching, batching, retry loops, or side channels.

Every binding must strictly adhere to the following developer experience and security guidelines:
- **Thin Translation Layer**: Keep the binding minimal. Each public operation should map directly to the underlying native function call.
- **Single Source of Truth for Constants**: Derive error codes and structure layouts from the generated C header (`slate.h`). Always verify the ABI version dynamically upon opening a database.
- **Distinct Security Errors**: Tamper detection and rollback protection errors must be surfaced as distinct, strongly typed errors in the host language. They must never be retried or flattened into generic I/O or system errors.
- **Surfaced Degradation**: Always expose the active security mode of the database so applications can transparently detect when rollback protection degrades on non-hardware-enforced storage.
- **Key Hygiene**: Root keys must be exactly 32 bytes. Bindings must pass them directly to the native interface and zero out any temporary host memory immediately after opening the database.
- **Safe Handle Lifetimes**: Require explicit database closure. Guard internal database pointers so that repeated closures or operations after closure fail cleanly with typed errors rather than causing memory corruption or crashes.
- **Null Buffer Safety**: Never pass null pointers for zero-length slices. Handle empty values safely using non-null dummy addresses or direct slice construction.
- **Matching libc**: The static library and the host runtime must be built against the **same libc**. Linking a glibc `libslate_kv_ffi.a` into a musl host (or the reverse) succeeds at link time and then faults at runtime — see [Gotchas](#gotchas-read-this-before-you-debug) below.
- **Stack Budget**: `slate_open` needs at least **52 KiB of stack** — measured, not estimated: running it on pthreads with bounded stacks gives SIGSEGV at 48 KiB and a clean return at 52 KiB, and every other operation fits inside the same budget, so mount is the peak. A normal OS thread has megabytes and never notices. A runtime that hands work to a coroutine, green thread, or fiber with a smaller fixed stack will fault on the first call into the engine. Either raise the stack or call from a full-sized thread.
- **Thread Synchronization**: All operations on a single database handle must be synchronized under a host-language mutex to ensure safe error reporting and race-free lifecycle checks.
- **No Side Channels**: Bindings must never inspect, modify, or parse the database storage files directly on the filesystem outside of the native C interface (with the sole exception of test suites simulating tamper scenarios).

## Quickstart

From a clean checkout, three commands get you a working binding:

```sh
make ffi-staticlib      # build libslate_kv_ffi.a + generate slate.h
make ffi-native-libs    # print the system libs YOUR platform needs
make bind-test          # build the lib, then run the Go conformance suite
```

To check the library itself before writing any binding code, run the
language-agnostic driver test — it drives the shared library through Python
`ctypes`, so it needs no Rust and no C compiler:

```sh
python3 artifacts/tools/slate_driver_test.py          # finds the library automatically
python3 artifacts/tools/slate_driver_test.py --lib target/release
```

44 checks covering round-trip, durability across reopen, the two-call `get`
protocol, invalid arguments, and tamper rejection. If this passes, the artifact
is sound and any failure in your binding is in the binding.

## Build Prerequisites & Workflow

Bindings link against the Rust static library (`libslate_kv_ffi.a`) by default, ensuring self-contained test binaries and compatibility across diverse environments.

### 1. Build the Native Static Library
Before building or testing any language binding, compile the native static library and generate the C header:
```sh
make ffi-staticlib
```
This produces:
- Header: `crates/slate-kv-ffi/include/slate.h`
- Static Library: `target/release/libslate_kv_ffi.a`

### 2. Native System Dependencies
Because Rust static libraries do not bundle operating system libraries, bindings must link against the required platform-specific system dependencies when compiling.
To discover the exact link flags required for your target architecture and operating system, run:
```sh
make ffi-native-libs
```
Typical system link dependencies:
- **macOS (Darwin)**: `-liconv -lSystem -lc -lm`
- **Linux (glibc)**: `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`
- **Linux (musl)**: `-lc -lm` (musl is largely self-contained)

## Gotchas (read this before you debug)

Every entry below cost real debugging time. They share a shape: the failure is
a **crash or a wrong answer with no diagnostic**, because a C ABI has no way to
tell you that you mismatched it.

### The libc of the archive must match the libc of the host

A Rust `staticlib` bakes in libc-dependent startup machinery at build time.
Linking a **glibc** archive into a **musl** host — or the reverse — succeeds at
link time and then misbehaves at runtime. Check and match before you debug
anything else:

```sh
tinygo info | grep 'LLVM triple'        # what libc is your host targeting?

rustup target add x86_64-unknown-linux-musl
cargo build -p slate-kv-ffi --release --target x86_64-unknown-linux-musl
export CGO_LDFLAGS="-L$PWD/target/x86_64-unknown-linux-musl/release -lslate_kv_ffi -lm -lpthread -ldl"
```

Note that a musl target **cannot produce a `cdylib`** — cargo warns `dropping
unsupported crate type cdylib` — so on musl the static archive is your only
option.

### TinyGo can compile this binding, but cannot run it

**Standard Go (`gc`) is the supported runtime.** TinyGo is supported as a
*compile* target only, and CI enforces exactly that: it builds the TinyGo test
binary and does not execute it.

This is not for want of trying. Every fallback in doc 017 §7 was attempted and
the outcome recorded in `DEVELOPMENT_STATE.md`:

| Attempt                                       | Outcome                                                                                               |
|-----------------------------------------------|-------------------------------------------------------------------------------------------------------|
| Explicit native libs on the TinyGo link line  | Still SIGSEGV                                                                                         |
| Link the `.so` instead of the `.a`            | Not possible on musl (`cdylib` unsupported); the glibc `.so` crashes inside glibc `malloc` under musl |
| Build the archive natively for musl           | Still SIGSEGV — so it is not a cross-libc mismatch                                                    |
| Raise the goroutine stack (`-stack-size=1MB`) | Identical crash — so it is not stack depth                                                            |

The crash is in the **first** test that opens and closes a database, i.e. at
the first real call into the engine. The remaining explanation is runtime
coexistence: TinyGo's Linux runtime brings its own GC (`gc.boehm`) and scheduler
(`scheduler.threads`), and hosting a Rust `std` library — with its own thread,
panic, and allocator machinery — inside it is not a combination either side
supports.

If you need SLATE on TinyGo, the tractable path is not to keep fighting the
static link: it is a `no_std` FFI variant that brings no Rust runtime with it.
That is real work, not a flag.

### The FFI must not use thread-local storage

The last-open-error slot is a process-wide `Mutex`, not a `thread_local!`, and
that is deliberate for a reason unrelated to TinyGo: **a thread-local gave the
wrong answer.** A C caller who opened a database on one thread and read
`slate_last_error_message` on another silently got an empty string, and nothing
in the C ABI promises thread affinity. Conformance case C15 pins this.

(Rust `std` itself uses TLS internally for panic state and stdio, so `nm | grep
tls` on the archive is *not* a useful audit — it will never be zero. The rule
applies to code you write in the FFI layer.)

### `slate_get` takes five parameters, not six

`vlen_inout` is **in/out**: set it to your buffer's capacity on the way in, and
read the value's true length back out. There is no separate capacity argument.

```c
uint8_t buf[64];
size_t  len = sizeof buf;                       /* in: capacity */
int rc = slate_get(db, key, klen, buf, &len);   /* out: true length */
```

Binding it with an extra argument segfaults on the first call. To query a size
without fetching, pass `NULL` for the buffer and `0` for the length.

### Never pass `NULL` for a zero-length slice

An empty value is legal and round-trips correctly, but the pointer must still
be valid. Use a one-byte dummy address rather than `NULL`.

### Profile constants: Pi is `0`, ESP32 is `1`

`SLATE_PROFILE_PI = 0`, `SLATE_PROFILE_ESP32 = 1`. Swapping them does not
error — you silently get the wrong flash geometry. Use the named constants from
`slate.h`, never a literal.

### `theta` in `slate_options` is accepted and ignored

The field exists in the struct, but `slate_open` never reads it: `THETA` is a
compile-time constant (16384). Setting it changes nothing. Do not expose it as
a working knob in your binding's options type without saying so.

### What the C surface does *not* expose

Twelve of the Rust `Db` methods have no C entry point: `delete_durable`,
`seal_epoch`, `scrub`, `stats`, `mount_report`, `write_amplification`,
`flash_bytes`, `len`, `is_empty`, `epoch`, `acked_seq`, `next_seq`. Practical
consequences for a binding author:

- **No durable delete in one call.** Do `slate_delete` then `slate_commit`.
- **No observability.** Write amplification and erase counts are unreachable
  from C, so a binding cannot report wear.
- **No iteration.** Not an ABI gap — the Rust engine has no `iter`/`scan`/
  `range` either. Do not promise range queries.

Two `Options` fields are also fixed by the C layer: `durability` is always
`Full` and `staleness_budget_ms` is always `1000`.

## Adding a New Language Binding

1. **Directory Naming**: Create a directory under `bind/` named strictly after the language (e.g., `go/`, `python/`, `node/`, `java/`).
2. **Manifest Ownership**: The binding directory must own its own package manifest, lockfile, and test harness. Note that `bind/` is excluded from the root Cargo workspace.
3. **Conformance Suite**: Every binding must implement and pass the full cross-language conformance suite defined in `bind/CONFORMANCE.md`.
4. **CI Integration**: Hook the binding's test suite into the root `Makefile` (`bind-test` target) and CI workflow.
