.PHONY: build
build:
	cargo +edos build -Z unstable-options --artifact-dir=../filesystem/bin/ --target x86_64-unknown-edos
	rm -f ../filesystem/bin/*.rlib ../filesystem/bin/*.a

.PHONY: build-release
build-release:
	cargo +edos build -Z unstable-options --artifact-dir=../filesystem/bin/ --release --target x86_64-unknown-edos
	rm -f ../filesystem/bin/*.rlib ../filesystem/bin/*.a

.PHONY: wm
wm:
	cargo +edos build --bin edos-wm --target x86_64-unknown-edos
	cp target/x86_64-unknown-edos/debug/edos-wm ../filesystem/bin/edos-wm

.PHONY: wm-release
wm-release:
	cargo +edos build --bin edos-wm --release --target x86_64-unknown-edos
	cp target/x86_64-unknown-edos/release/edos-wm ../filesystem/bin/edos-wm

.PHONY: wintest
wintest:
	cargo +edos build --bin wintest --target x86_64-unknown-edos
	cp target/x86_64-unknown-edos/debug/wintest ../filesystem/bin/wintest

.PHONY: wintest-release
wintest-release:
	cargo +edos build --bin wintest --release --target x86_64-unknown-edos
	cp target/x86_64-unknown-edos/release/wintest ../filesystem/bin/wintest

.PHONY: check
check:
	cargo +edos check --target x86_64-unknown-edos

.PHONY: fmt
fmt:
	cargo +edos fmt --target x86_64-unknown-edos

.PHONY: clean
clean:
	cargo +edos clean
