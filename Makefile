.PHONY: build release fmt lint unit e2e contract terminal all install dist package-check
BAZEL ?= bazelisk
BAZEL_STARTUP_FLAGS ?= --output_user_root=$(CURDIR)/.build/bazel
BAZEL_TEST = $(BAZEL) $(BAZEL_STARTUP_FLAGS) test --test_output=errors
build:
	cargo build --locked
release:
	cargo build --release --locked --bin bazelqueue
fmt:
	cargo fmt --all -- --check
lint:
	cargo clippy --all-targets --all-features --locked -- -D warnings
unit:
	$(BAZEL_TEST) //:unit
e2e:
	$(BAZEL_TEST) //:e2e
contract:
	test -n "$(BAZELQUEUE_TEST_BAZEL)"
	test -n "$(BAZELQUEUE_TEST_VERSION)"
	$(BAZEL_TEST) //:native --test_env=BAZELQUEUE_TEST_BAZEL --test_env=BAZELQUEUE_TEST_VERSION
terminal:
	$(BAZEL_TEST) //:terminal
package-check:
	test -n "$(ARCHIVE)" -a -n "$(VERSION)"
	$(BAZEL_TEST) //:package --test_env=BAZELQUEUE_TEST_ARCHIVE=$(abspath $(ARCHIVE)) --test_env=BAZELQUEUE_TEST_VERSION=$(VERSION)
all: fmt lint
	$(BAZEL_TEST) //:validation
install: release
	./target/release/bazelqueue setup
dist:
	dist build --artifacts=local --target=aarch64-apple-darwin
