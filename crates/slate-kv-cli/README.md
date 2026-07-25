# slate-kv-cli

[![crates.io](https://img.shields.io/crates/v/slate-kv-cli.svg)](https://crates.io/crates/slate-kv-cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**A shell for [SLATE](https://github.com/ja7ad/slate) databases.** Read, write, delete and inspect a store from the command line — for debugging a device, scripting a provisioning step, or checking what a running application actually persisted.

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

`<db_dir>` is the **directory** holding `data.bin` and `counter.bin` — the same path you passed to `Db::open`. It is created if it does not exist.

```sh
slate-kv-cli put   ./slate_db sensor_1 "23.5 C"
slate-kv-cli get   ./slate_db sensor_1
slate-kv-cli del   ./slate_db sensor_1
slate-kv-cli stats ./slate_db
```

Writes go through `put_durable` / `delete_durable`, so a command that exits `0` has already committed. Nothing is left buffered when the process ends.

### The `hex_key` argument

Every subcommand takes an optional trailing 64-character hex string: the 32-byte root key the database was created with.

```sh
slate-kv-cli get ./slate_db sensor_1 \
  4242424242424242424242424242424242424242424242424242424242424242
```

Omit it and the CLI uses the all-`0x42` development key — convenient for local testing, and the reason you should not use that key for anything real.

Anything that is not exactly 64 hex characters is silently ignored and the default key is used instead. A wrong key does not produce a "wrong password" error: the AEAD tag check fails and you get a tamper/mount error. That is the correct behaviour for an authenticated store — it cannot distinguish "wrong key" from "altered data" — but it does mean a mount failure is worth double-checking against the key you passed.

Note that the key appears in your shell history and in `ps` output. For anything beyond development, prefer a program that reads the key from a keystore via [`slate-kv`](https://crates.io/crates/slate-kv)'s `KeySource::File` or `KeySource::Env`.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Usage error, open failure, or operation failure |
| 2 | `get` only: key not found |

The distinct `2` is there so scripts can branch on absence without parsing stderr:

```sh
if val=$(slate-kv-cli get ./slate_db sensor_1 2>/dev/null); then
    echo "value: $val"
elif [ $? -eq 2 ]; then
    echo "not set yet"
fi
```

`get` prints the value to **stdout** and nothing else; all diagnostics go to stderr. Values are printed with lossy UTF-8 conversion, so binary values will not round-trip through the shell.

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
...
```

The first block is read from flash and is the real persisted state. **The second block is per-process**: the CLI opens the database, does one operation, and exits, so the counters describe that one invocation — not the history of the store. They are useful for seeing what a single command costs, not for auditing a deployment.

`Security Mode` reports what the store can actually guarantee. On an ordinary filesystem it says `BestEffortRollback`: the log is tamper-evident, but a file-backed counter cannot stop someone restoring an older `data.bin` + `counter.bin` pair. `Full` requires a hardware anchor such as an eFuse or TPM counter.

## Scope

Deliberately small: no shell mode, no range scans, no import/export, no config file. It exists to inspect and poke a store. For anything richer, use [`slate-kv`](https://crates.io/crates/slate-kv) directly — the CLI is roughly 150 lines over that crate's public API, and is a fine template to copy.

## License

Dual-licensed under [MIT](https://github.com/ja7ad/slate/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/ja7ad/slate/blob/main/LICENSE-APACHE), at your option.
