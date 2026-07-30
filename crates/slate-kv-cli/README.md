# slate-kv-cli

[![crates.io](https://img.shields.io/crates/v/slate-kv-cli.svg)](https://crates.io/crates/slate-kv-cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**A shell for [SLATE](https://github.com/ja7ad/slate) databases.** Read, write, delete and inspect a store from the command line — for debugging a device, scripting a provisioning step, or checking what a running application actually persisted.

A thin wrapper over [`slate-kv`](https://crates.io/crates/slate-kv)'s public API; the semantics it exposes are that crate's, and the on-flash format is specified in [`../../docs/specification.md`](../../docs/specification.md).

## Install

```sh
cargo install slate-kv-cli
```

Or from a checkout:

```sh
cargo run -p slate-kv-cli -- stats ./slate_db
```

## Usage

```text
slate-kv-cli <SUBCOMMAND> <DB_DIR> [ARGS...]

    get   <db_dir> <key> [hex_key]         Read the value for a key
    put   <db_dir> <key> <val> [hex_key]   Write a key/value pair and commit
    del   <db_dir> <key> [hex_key]         Delete a key and commit
    stats <db_dir> [hex_key]               Show database state and session metrics
    help                                   Show usage
```

`<db_dir>` is the **directory** holding `data.bin` and `counter.bin` — the same path you passed to `Db::open`. It is created if it does not exist, so a typo in the path yields a new empty database rather than an error.

```sh
slate-kv-cli put   ./slate_db sensor_1 "23.5 C"
slate-kv-cli get   ./slate_db sensor_1
slate-kv-cli del   ./slate_db sensor_1
slate-kv-cli stats ./slate_db
```

Every command opens the database with `Options::default()` — a 4 MiB arena, `b_commit` 8, automatic batch sizing, the Pi energy profile, and full `fsync` durability. There is no flag to change any of those; a store created with a different capacity or key count should be opened by a program that passes matching `Options`.

Both mutating commands go through `slate-kv`'s durable entry points, and dropping the `Db` at process exit performs one final commit. That final commit is **best-effort**: its error is discarded, so an I/O failure at drop time still exits `0`. In practice a `0` exit means the write reached flash, but if you need that guaranteed rather than typical — an unattended provisioning script, say — verify by reading the key back in a separate invocation rather than trusting the exit status alone.

### The `hex_key` argument

Every subcommand takes an optional trailing 64-character hex string: the 32-byte root key the database was created with.

```sh
slate-kv-cli get ./slate_db sensor_1 \
  4242424242424242424242424242424242424242424242424242424242424242
```

Omit it and the CLI uses the all-`0x42` development key — convenient for local testing, and the reason you should not use that key for anything real.

Two behaviours to be aware of. **Anything that is not exactly 64 hex characters is silently ignored** and the default key is used instead — a typo'd or truncated key does not produce a complaint, it produces a mount failure. And a wrong key does not report "wrong password": the AEAD tag check fails and you get a tamper or mount error. That is correct for an authenticated store — it genuinely cannot distinguish "wrong key" from "altered data" — but it means a mount failure is always worth double-checking against the key you passed before concluding the data was attacked.

Note also that the key appears in your shell history and in `ps` output. For anything beyond development, prefer a program that reads the key from a keystore via [`slate-kv`](https://crates.io/crates/slate-kv)'s `KeySource::File` or `KeySource::Env`, neither of which this CLI exposes.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Usage error, open failure, or operation failure |
| 2 | `get` only: key not found |

The distinct `2` lets scripts branch on absence without parsing stderr:

```sh
if val=$(slate-kv-cli get ./slate_db sensor_1 2>/dev/null); then
    echo "value: $val"
elif [ $? -eq 2 ]; then
    echo "not set yet"
fi
```

`get` prints the value to **stdout** and nothing else; all diagnostics go to stderr. Values are printed with lossy UTF-8 conversion, so binary values do not round-trip through the shell — a value containing invalid UTF-8 comes back with replacement characters, silently. Keys and values are taken as raw argument bytes, so there is no escaping mechanism either.

### `stats`

```text
=== Persistent Database State ===
Path:          ./slate_db
Epoch:         3
Acked Seq:     1841
Next Seq:      1842
Active Keys:   512
Security Mode: BestEffortRollback

=== Current Session Metrics ===
Commits:       1
Wakes:         1
User Bytes:    0
GC Bytes:      0
Parity Bytes:  0
Ckpt Bytes:    0
Erases:        0
```

The first block is read from flash and is the real persisted state. **The second block is per-process**: the CLI opens the database, does one operation, and exits, so the counters describe that one invocation — not the history of the store. They are useful for seeing what a single command costs, not for auditing a deployment. (SLATE keeps no cumulative on-flash counters, so there is nothing the CLI could print instead.)

`Security Mode` reports what the store can actually guarantee. On an ordinary filesystem it says `BestEffortRollback`: the log is tamper-evident, but a file-backed counter cannot stop someone restoring an older `data.bin` + `counter.bin` pair. `Full` requires a hardware anchor such as an eFuse or TPM counter.

## Scope

Deliberately small: no shell mode, no range scans (the engine has no iteration API), no import/export, no config file, no `compact`, no `seal_epoch`, and no way to set any `Options` field. It exists to inspect and poke a store. For anything richer, use [`slate-kv`](https://crates.io/crates/slate-kv) directly — this CLI is about 150 lines over that crate's public API, and is a fine template to copy.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
