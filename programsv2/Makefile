.PHONY: build
build:
	cargo build -Z unstable-options --artifact-dir=../filesystem/bin/

.PHONY: build-release
build-release:
	cargo build -Z unstable-options --artifact-dir=../filesystem/bin/ --release

.PHONY: check
check:
	cargo check

.PHONY: fmt
fmt:
	cargo fmt

.PHONY: clean
clean:
	cargo clean
