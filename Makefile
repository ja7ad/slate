.PHONY: all fmt lint test build-bare build-esp check-all clean ffi-staticlib ffi-native-libs bind-test

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
	cargo build -p slate-kv-core -p slate-kv-hal -p slate-kv-crypto -p slate-kv-erasure --no-default-features --target thumbv7em-none-eabihf

build-esp:
	cd targets/esp32 && cargo build --release --no-default-features \
		--features chip-esp32c3,counter-flash --target riscv32imc-unknown-none-elf

check-all: fmt-check lint test build-bare build-esp ffi-staticlib bind-test
	@echo "All checks passed successfully!"

ffi-staticlib:
	cargo build -p slate-kv-ffi --release

ffi-native-libs:
	cargo rustc -p slate-kv-ffi --release --crate-type staticlib -- --print native-static-libs

bind-test: ffi-staticlib
	@if [ -d bind/go ]; then cd bind/go && LD_LIBRARY_PATH=../../target/release DYLD_FALLBACK_LIBRARY_PATH=../../target/release go test -v ./...; fi

clean:
	cargo clean
	cd targets/esp32 && cargo clean
	@if [ -d bind/go ]; then cd bind/go && go clean -testcache; fi
