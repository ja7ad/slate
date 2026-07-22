# SLATE

SLATE (Secure, Log-structured, Authenticated, Tamper-Evident) is a single-device key–value storage engine designed for the edge regime (microcontrollers to Raspberry Pi).

## std Target (Raspberry Pi / Desktop)

SLATE ships with a `std` wrapper targeting Linux-class boards via `FileFlash` (file-backed flash emulation) and `FileCounter` (best-effort degradation). 

**Honesty Note (§9.3)**: If you are deploying to a Raspberry Pi or an OS-backed server and do *not* require SLATE's specific bundle of guarantees (tamper-evident log structure, cryptographically enforced prefix-durability, and exact bounds on recovery), you should strongly consider using a mature embedded database like SQLite or RocksDB. They benefit from decades of OS-level optimizations. SLATE is intended for edge and embedded scenarios where security against active adversaries and deterministic memory behavior trump raw disk throughput.
