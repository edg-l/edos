# Nuke built-in rules and variables.
MAKEFLAGS += -rR
.SUFFIXES:

# This is the name that our final executable will have.
# Change as needed.
override OUTPUT := kernel

# Convenience macro to reliably declare user overridable variables.
override USER_VARIABLE = $(if $(filter $(origin $(1)),default undefined),$(eval override $(1) := $(2)))

# Target architecture to build for. Default to x86_64.
$(call USER_VARIABLE,KARCH,x86_64)

ifeq ($(RUST_TARGET),)
    override RUST_TARGET := $(KARCH)-unknown-none
	ifeq ($(KARCH),riscv64)
    	override RUST_TARGET := riscv64gc-unknown-none-elf
	endif
endif

ifeq ($(RUST_PROFILE),)
    override RUST_PROFILE := dev
endif

override RUST_PROFILE_SUBDIR := $(RUST_PROFILE)
ifeq ($(RUST_PROFILE),dev)
    override RUST_PROFILE_SUBDIR := debug
endif

# Default target.
.PHONY: all
all:
	RUSTFLAGS="-C relocation-model=static -C force-frame-pointers=yes" cargo build --target $(RUST_TARGET) --profile $(RUST_PROFILE) $(CARGO_FLAGS)
	cp target/$(RUST_TARGET)/$(RUST_PROFILE_SUBDIR)/edos-kernel kernel


.PHONY: check
check: check-features
	RUSTFLAGS="-C relocation-model=static -C force-frame-pointers=yes" cargo check --target $(RUST_TARGET)

# Every optional feature, one at a time. Code behind a `cfg(feature)` the
# default build never compiles rots silently: `util/trace.rs` kept calling a
# `uart_16550` item that had been gone since the crate's 0.6 API change, and
# nothing said so. The list is read out of Cargo.toml, so a new feature is
# covered without touching this rule.
.PHONY: check-features
check-features:
	@for f in $$(sed -n '/^\[features\]/,/^\[/p' Cargo.toml | sed -n 's/^\([a-z0-9-]*\) *=.*/\1/p' | grep -v '^default$$'); do \
		echo "  check --features $$f"; \
		RUSTFLAGS="-C relocation-model=static -C force-frame-pointers=yes" \
			cargo check --quiet --target $(RUST_TARGET) --features "$$f" || exit 1; \
	done

# Clippy over the default build and every optional feature, denying warnings so
# a lint is a red gate rather than a line nobody reads. Same feature sweep as
# `check-features`, and for the same reason: a lint in code behind a
# `cfg(feature)` is invisible to the default build.
.PHONY: clippy
clippy:
	RUSTFLAGS="-C relocation-model=static -C force-frame-pointers=yes" \
		cargo clippy --target $(RUST_TARGET) -- -D warnings
	@for f in $$(sed -n '/^\[features\]/,/^\[/p' Cargo.toml | sed -n 's/^\([a-z0-9-]*\) *=.*/\1/p' | grep -v '^default$$'); do \
		echo "  clippy --features $$f"; \
		RUSTFLAGS="-C relocation-model=static -C force-frame-pointers=yes" \
			cargo clippy --quiet --target $(RUST_TARGET) --features "$$f" -- -D warnings || exit 1; \
	done

# Remove object files and the final executable.
.PHONY: clean
clean:
	cargo clean
	rm -rf kernel

.PHONY: distclean
distclean: clean


.PHONY: fmt
fmt:
	cargo fmt
