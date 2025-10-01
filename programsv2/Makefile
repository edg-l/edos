.PHONY: build
build:
	cargo +edos build -Z unstable-options --artifact-dir=../filesystem/bin/ --target x86_64-unknown-edos

.PHONY: build-release
build-release:
	cargo +edos build -Z unstable-options --artifact-dir=../filesystem/bin/ --release --target x86_64-unknown-edos

.PHONY: check
check:
	cargo +edos check --target x86_64-unknown-edos

.PHONY: fmt
fmt:
	cargo +edos fmt --target x86_64-unknown-edos

.PHONY: clean
clean:
	cargo +edos clean
