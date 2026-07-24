.PHONY: all fmt lint test build-bare build-esp check-all clean

all: check-all

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

build-bare:
	cargo build -p slate-core -p slate-hal -p slate-crypto -p slate-erasure --no-default-features --target thumbv7em-none-eabihf

build-esp:
	cd targets/esp32 && cargo build --release --no-default-features \
		--features chip-esp32c3,counter-flash --target riscv32imc-unknown-none-elf

check-all: fmt-check lint test build-bare build-esp
	@echo "All checks passed successfully!"

clean:
	cargo clean
	cd targets/esp32 && cargo clean
