.PHONY: build release fmt lint unit e2e contract terminal all install dist
build:
	cargo build --locked
release:
	cargo build --release --locked --bin bazelqueue
fmt:
	cargo fmt --all -- --check
lint:
	cargo clippy --all-targets --all-features --locked -- -D warnings
unit:
	cargo test --locked
e2e:
	CARGO_TARGET_DIR=target/e2e cargo test --features test-fixtures --locked -- --test-threads=1
contract:
	test -n "$(BAZELQUEUE_TEST_BAZEL)"
	CARGO_TARGET_DIR=target/e2e cargo test --features test-fixtures --test e2e native_ --locked -- --ignored --test-threads=1
terminal:
	CARGO_TARGET_DIR=target/e2e cargo build --features test-fixtures --bins --locked
	BAZELQUEUE_TEST_BINARY=$(CURDIR)/target/e2e/debug/bazelqueue BAZELQUEUE_TEST_FIXTURE=$(CURDIR)/target/e2e/debug/fixture-backend python3 tests/terminal_e2e.py
all: fmt lint unit e2e terminal
install: release
	./target/release/bazelqueue setup
dist:
	dist build --artifacts=local --target=aarch64-apple-darwin
