

.PHONY: build
build:
	cargo build -Z unstable-options --artifact-dir=out --target=x86_64-edos.json

.PHONY: build-release
build-release:
	cargo build --release -Z unstable-options --artifact-dir=out --target=x86_64-edos.json

.PHONY: check
check:
	cargo check -Z unstable-options --artifact-dir=out --target=x86_64-edos.json

.PHONY: fmt
fmt:
	cargo fmt

.PHONY: clean
clean:
	cargo clean
