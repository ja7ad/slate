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
- **Thread Synchronization**: All operations on a single database handle must be synchronized under a host-language mutex to ensure safe error reporting and race-free lifecycle checks.
- **No Side Channels**: Bindings must never inspect, modify, or parse the database storage files directly on the filesystem outside of the native C interface (with the sole exception of test suites simulating tamper scenarios).

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

## Adding a New Language Binding

1. **Directory Naming**: Create a directory under `bind/` named strictly after the language (e.g., `go/`, `python/`, `node/`, `java/`).
2. **Manifest Ownership**: The binding directory must own its own package manifest, lockfile, and test harness. Note that `bind/` is excluded from the root Cargo workspace.
3. **Conformance Suite**: Every binding must implement and pass the full cross-language conformance suite defined in `bind/CONFORMANCE.md`.
4. **CI Integration**: Hook the binding's test suite into the root `Makefile` (`bind-test` target) and CI workflow.
