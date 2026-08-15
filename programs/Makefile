# Installed binaries are stripped; the unstripped ones stay in `target/`.
#
# `--artifact-dir` hardlinks into `../filesystem/bin`, so the copy there and the
# one under `target/x86_64-unknown-edos/` are the same inode. Stripping in place
# would therefore throw away the debug info `addr2line` needs. `objcopy` writes a
# new file and the `mv` replaces the directory entry, which breaks the link and
# leaves `target/`'s copy whole.
#
# `--strip-debug` rather than `--strip-all`: it takes edos-wm from 7.6M to 644K
# where stripping everything reaches 557K, and keeping `.symtab` is worth more
# than the last 13%. Debug builds are stripped too, because nothing reads a guest
# binary's debug info: `addr2line` is pointed at `target/`, `strace` names calls
# from the kernel's own table, and there is no debugger in the guest. Unstripped
# binaries only made the images seven times larger.
define strip_installed
	@for f in ../filesystem/bin/*; do \
		[ -f "$$f" ] || continue; \
		objcopy --strip-debug "$$f" "$$f.stripped" && mv -f "$$f.stripped" "$$f"; \
	done
endef

# One binary, for the per-program targets. $(1) is the path under target/.
define install_one
	objcopy --strip-debug $(1) ../filesystem/bin/$(notdir $(1))
endef

# Programs published through `grab` instead of going on the boot image. The
# build emits every workspace binary into ../filesystem/bin, so keeping one off
# the image is a move afterwards rather than a change to what gets built. They
# are moved after stripping, so what is published is what would have shipped.
#
# Read from the same `shipped = false` key `tools/grab-repo` reads, rather than
# listed here as well: a program is packaged or it is not, and two places to say
# so is one place for them to disagree. An absent key means shipped, which is
# what makes adding a `pkg.toml` for metadata alone safe. Overridable on the
# command line (`make PACKAGED=`) to put everything on the image for a test.
PACKAGED := $(patsubst %/pkg.toml,%,$(shell grep -lE '^[[:space:]]*shipped[[:space:]]*=[[:space:]]*false' */pkg.toml 2>/dev/null))

define stage_packaged
	@mkdir -p ../pkgstage/bin
	@for p in $(PACKAGED); do \
		if [ -f "../filesystem/bin/$$p" ]; then \
			mv -f "../filesystem/bin/$$p" "../pkgstage/bin/$$p"; \
		fi; \
	done
endef

.PHONY: build
build:
	cargo +edos build -Z unstable-options --artifact-dir=../filesystem/bin/ --target x86_64-unknown-edos
	rm -f ../filesystem/bin/*.rlib ../filesystem/bin/*.a
	$(strip_installed)
	$(stage_packaged)

.PHONY: build-release
build-release:
	cargo +edos build -Z unstable-options --artifact-dir=../filesystem/bin/ --release --target x86_64-unknown-edos
	rm -f ../filesystem/bin/*.rlib ../filesystem/bin/*.a
	$(strip_installed)
	$(stage_packaged)

.PHONY: wm
wm:
	cargo +edos build --bin edos-wm --target x86_64-unknown-edos
	$(call install_one,target/x86_64-unknown-edos/debug/edos-wm)

.PHONY: wm-release
wm-release:
	cargo +edos build --bin edos-wm --release --target x86_64-unknown-edos
	$(call install_one,target/x86_64-unknown-edos/release/edos-wm)

.PHONY: wintest
wintest:
	cargo +edos build --bin wintest --target x86_64-unknown-edos
	$(call install_one,target/x86_64-unknown-edos/debug/wintest)

.PHONY: wintest-release
wintest-release:
	cargo +edos build --bin wintest --release --target x86_64-unknown-edos
	$(call install_one,target/x86_64-unknown-edos/release/wintest)

.PHONY: check
check:
	cargo +edos check --target x86_64-unknown-edos

.PHONY: fmt
fmt:
	cargo +edos fmt --target x86_64-unknown-edos

.PHONY: clean
clean:
	cargo +edos clean
