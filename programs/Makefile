

.PHONY: build
build:
	cargo build -Z unstable-options -Z json-target-spec --artifact-dir=../filesystem/bin/ --target=x86_64-edos.json

.PHONY: build-release
build-release:
	cargo build --release -Z unstable-options -Z json-target-spec --artifact-dir=../filesystem/bin/ --target=x86_64-edos.json

.PHONY: check
check:
	cargo check -Z unstable-options -Z json-target-spec --artifact-dir=../filesystem/bin/ --target=x86_64-edos.json

.PHONY: fmt
fmt:
	cargo fmt

.PHONY: clean
clean:
	cargo clean
