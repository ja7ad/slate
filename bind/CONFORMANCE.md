# SLATE Language Bindings Conformance Suite

This document defines the standard conformance test suite required for every language binding under `bind/`.

Every binding must implement all of the following test cases in its native test framework, using these exact IDs as test function names. This ensures uniform behavioral verification and makes test results directly comparable across different programming languages.

| ID | Scenario | Expected Result |
|---|---|---|
| C1 | Open a fresh database directory with a 32-byte key, `ProfilePi`, and `BCommit=1` | Successfully returns a valid, non-null database handle |
| C2 | Perform a durable write (`PutDurable`) followed by a read (`Get`) | The retrieved value bytes match the written bytes exactly |
| C3 | Attempt to read (`Get`) an absent key | Returns a key-not-found error mapped idiomatically to the host language |
| C4 | Read into a 0-capacity buffer, then read into a buffer of the exact size | Returns buffer-too-small error along with required byte length; subsequent read succeeds |
| C5 | Delete a key (`Delete`) and attempt to read it (`Get`) | Returns key-not-found error |
| C6 | Write to buffer (`Put`), commit (`Commit`), close, reopen, and read (`Get`) | The written value survives reopening and is retrieved successfully |
| C7 | Write and read a zero-length value | Successfully round-trips an empty value without null pointer errors |
| C8 | Close, modify bytes in `counter.bin` to simulate tampering, and reopen | Returns a distinct tamper-detected error, distinguishable from generic I/O errors |
| C9 | Query security mode on file-backed storage (`SecurityMode`) | Returns best-effort rollback protection mode (`SecurityModeBestEffortRollback`) |
| C10 | Close the handle twice; attempt any operation after closing | Returns a typed closed-database error without crashing or memory corruption |
| C11 | Inspect error message after a failed operation or failed database opening | Returns a non-empty, descriptive error string explaining the failure |
| C12 | Pass an invalid key length (not 32 bytes) when opening | Rejected cleanly by the binding before invoking the native C interface |
| C13 | Check dynamic ABI version against binding expectation | Major version matches expected ABI version |

**Security Error Note**: Case C8 is the critical verification test for storage security. It proves that the language binding preserves the specific identity of tamper detection errors rather than flattening them into general I/O or system exceptions. A binding must explicitly assert the distinct tamper error type to pass this suite.
