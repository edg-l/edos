# The EDOS half of a newlib port.
#
# Produces libgloss-edos.a and crt0.o, which together with newlib's libc.a are
# everything a C program needs to run on EDOS.
#
# NEWLIB points at an installed newlib built for a bare x86-64 target with
# --disable-newlib-supplied-syscalls, which is what makes newlib call the
# unprefixed names this layer defines. See README.md for the recipe.

NEWLIB ?= $(HOME)/dev/newlib-install/x86_64-unknown-elf
LLVM   ?= /usr/lib/llvm-21/bin

CC := $(LLVM)/clang
AR := $(LLVM)/llvm-ar

TARGET := x86_64-unknown-elf

CFLAGS := --target=$(TARGET) -ffreestanding -fno-stack-protector -fno-pie \
          -I$(NEWLIB)/include -I. -Wall -Wextra -O2

OBJS := syscalls.o
OUT  := libgloss-edos.a crt0.o

all: $(OUT)

libgloss-edos.a: $(OBJS)
	$(AR) rcs $@ $^

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

# A C program for EDOS: crt0, the program, then newlib with this layer behind
# it. libc is named twice because newlib calls back into these stubs, and the
# linker resolves an archive only against what is already undefined.
LDLIBS := -L$(NEWLIB)/lib -L. -lc -lgloss-edos -lc -lm

test/ctest: test/ctest.o crt0.o libgloss-edos.a
	$(LLVM)/ld.lld -o $@ --no-dynamic-linker -e _start crt0.o test/ctest.o $(LDLIBS)

# Install the test into the image tree so `make sata-disk.img` picks it up.
install-test: test/ctest
	cp test/ctest ../../filesystem/bin/ctest

clean:
	rm -f $(OBJS) $(OUT) test/ctest.o test/ctest

.PHONY: all clean install-test
