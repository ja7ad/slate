# Contributing to SLATE

Thank you for your interest in contributing to **SLATE**! We welcome bug reports, feature requests, documentation improvements, and code contributions.

SLATE is designed to be a provably secure, ultra-light, and zero-heap key–value engine. To maintain these guarantees, all contributions must adhere to the design principles and guidelines below.

---

## Code of Conduct & Development Principles

### 1. `no_std` and Zero Heap Discipline
- Core crates ([`slate-core`](crates/slate-core), [`slate-crypto`](crates/slate-crypto), [`slate-erasure`](crates/slate-erasure), [`slate-hal`](crates/slate-hal)) are `#![no_std]` and **zero allocation**. No `alloc`, `Vec`, `Box`, or `String` in the core engine.
- RAM working sets must stay bounded and caller-provided via fixed buffers.
- `#![forbid(unsafe_code)]` is enforced across core crates. Any exception requires explicit justification and review.
- No floating-point math (`#![deny(clippy::float_arithmetic)]`) in `slate-core`.

### 2. Durability & Crash Safety
- Writes are acknowledged **only** after both record pages and twin commit markers are durably programmed.
- Every record read from storage must be AEAD tag-verified before accessing any header or payload fields.

---

## Development Workflow & Verification

Before submitting a pull request, ensure all local checks pass:

```bash
# 1. Format code
cargo fmt --all --check

# 2. Run Clippy lints (warnings treated as errors)
cargo clippy --workspace --all-targets -- -D warnings

# 3. Run unit and integration test suite
cargo test --workspace

# 4. Verify no_std bare-metal build purity
cargo build -p slate-core -p slate-hal -p slate-crypto -p slate-erasure --no-default-features --target thumbv7em-none-eabihf

# 5. Build ESP32 target firmware (requires Xtensa toolchain)
cd targets/esp32 && cargo build --release --target xtensa-esp32-none-elf

# 6. Install Espressif QEMU and run the crash test suite
sudo apt-get install -y libsdl2-2.0-0 libslirp0 libglib2.0-0 libpixman-1-0 libgcrypt20 python3-serial
wget https://github.com/espressif/qemu/releases/download/esp-develop-9.2.2-20260417/qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz
tar -xf qemu-riscv32-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz
export PATH="$PWD/qemu/bin:$PATH"
cd targets/esp32
./scripts/qemu_crash.sh --iters 25 --attack none
./scripts/qemu_crash.sh --iters 2 --attack rollback
./scripts/qemu_crash.sh --iters 2 --attack tamper
```

You can also use the provided `Makefile` to run these commands quickly:
```bash
make fmt       # Format code
make lint      # Run Clippy lints
make test      # Run tests
make build-esp # Build ESP32 target
make check-all # Run all checks
```


---

## Commit Message Guidelines

We follow the [Conventional Commits](https://www.conventionalcommits.org/) specification for commit messages:

```
<type>(<scope>): <short summary>
```

### Types:
- `feat`: A new feature
- `fix`: A bug fix
- `docs`: Documentation changes
- `style`: Code style / formatting changes (no functional logic change)
- `refactor`: Code restructuring without changing behavior
- `perf`: Performance optimization
- `test`: Adding or updating tests
- `chore`: Build scripts, dependencies, or CI updates

### Scopes:
- `core`, `crypto`, `erasure`, `hal`, `std`, `ffi`, `sim`, `esp32`, `ci`, `docs`

*Example:* `feat(core): implement integer Newton isqrt for B* commit scheduler`

---

## Submitting Pull Requests

1. Fork the repository and create a feature branch off `develop`.
2. Keep your commits clean and self-contained.
3. Ensure all tests, Clippy checks, and `no_std` build checks pass cleanly.
4. Open a Pull Request targeting the `develop` branch with a clear description of the changes.

---

## Licensing Notice

By contributing to SLATE, you agree that your contributions will be licensed under the project's dual license (**MIT OR Apache-2.0**).
