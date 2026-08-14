# libgloss-edos

The EDOS half of a newlib port: the operating-system layer that a C library is
written against, plus the process entry that gets a C program started.

With this, C software compiles and runs on EDOS. `libs/libgloss-edos/test/ctest.c`
exercises it through newlib — `malloc`, `fopen`/`fread`/`fwrite`, `stat`,
`fseek`, `errno`, `gettimeofday`, `qsort`, `strtod`, float `printf` — and passes
15 of 15 in the guest.

## What is here

| file | what it is |
| --- | --- |
| `edos_syscall.h` | raw syscall entry, the error window, and the two translation tables |
| `syscalls.c` | the 19 functions newlib calls out to |
| `crt0.c` | `_start`: the SysV initial stack into `main(argc, argv, envp)` |
| `test/ctest.c` | the regression suite, run in the guest |

## Building newlib

newlib is not vendored. Build it once against a bare x86-64 target, with clang
as the cross compiler — no gcc cross-toolchain is needed.

```bash
git clone --depth 1 https://sourceware.org/git/newlib-cygwin.git ~/dev/newlib
mkdir -p ~/dev/newlib-build && cd ~/dev/newlib-build
../newlib/configure \
  --target=x86_64-unknown-elf \
  --prefix=$HOME/dev/newlib-install \
  --disable-newlib-supplied-syscalls \
  --disable-multilib \
  --enable-newlib-io-c99-formats \
  --enable-newlib-io-long-long \
  --enable-newlib-io-float \
  CC_FOR_TARGET="/usr/lib/llvm-21/bin/clang --target=x86_64-unknown-elf -ffreestanding -fno-stack-protector -D__SCHAR_WIDTH__=8 -D__LONG_LONG_WIDTH__=64" \
  AR_FOR_TARGET=/usr/lib/llvm-21/bin/llvm-ar \
  RANLIB_FOR_TARGET=/usr/lib/llvm-21/bin/llvm-ranlib \
  AS_FOR_TARGET="/usr/lib/llvm-21/bin/clang --target=x86_64-unknown-elf -c"
make MAKEINFO=true -j$(nproc) && make MAKEINFO=true install
```

Four of those flags are load-bearing and were each found by something failing:

- **`--disable-newlib-supplied-syscalls`** is what makes newlib call `read`,
  `write`, `open` and the rest by their unprefixed names and supply none of them
  itself. Without it newlib brings its own stubs and this layer is ignored.
- **`--enable-newlib-io-c99-formats`** — without it `printf` does not understand
  `%zu`. It prints a literal `zu` *and does not consume the argument*, so every
  later conversion reads the wrong slot. A `%s` after a `%zu` then dereferences
  an integer: the first symptom here was a page fault at address `0x12`, which
  was a byte count of 18 being used as a pointer. Any real C program will hit
  this.
- **`-D__SCHAR_WIDTH__=8 -D__LONG_LONG_WIDTH__=64`** because clang does not
  predefine those two, and newlib's `stdbit.h` implementation needs them.
- **`MAKEINFO=true`** stubs out a documentation build that wants `makeinfo`.
  Nothing else in the tree needs it.

## Building this layer

```bash
make -C libs/libgloss-edos                 # libgloss-edos.a and crt0.o
make -C libs/libgloss-edos install-test    # builds test/ctest into filesystem/bin/
```

`NEWLIB` and `LLVM` are overridable if either lives somewhere else.

## Compiling a C program for EDOS

```bash
NEWLIB=$HOME/dev/newlib-install/x86_64-unknown-elf
clang --target=x86_64-unknown-elf -ffreestanding -fno-stack-protector -fno-pie \
      -I$NEWLIB/include -O2 -c -o prog.o prog.c
ld.lld -o prog --no-dynamic-linker -e _start \
      libs/libgloss-edos/crt0.o prog.o \
      -L$NEWLIB/lib -L libs/libgloss-edos -lc -lgloss-edos -lc -lm
```

`-lc` appears twice because newlib calls back into this layer and a linker
resolves an archive only against what is already undefined at that point.

The result is `ET_EXEC`, statically linked, with no relocations — which is what
the EDOS loader handles today. `-fno-pie` matters: an `ET_DYN` image would need
the loader's `R_X86_64_RELATIVE` path and gains nothing here.

## The two things that do not map

The design note said every one of newlib's 19 functions lands on a syscall that
already exists, and that is true. What does not carry across is the
**constants**, and both mismatches are silent.

**Open flags.** `O_RDONLY`, `O_WRONLY` and `O_RDWR` are 0, 1 and 2 on both
sides. Nothing above them agrees: newlib's `O_CREAT` is `0x200` where the kernel
uses `0x40`, and newlib's `O_TRUNC` is `0x400`, which the kernel reads as
`O_APPEND`. Passing the value through unchanged makes `fopen(path, "w")` fail
with `ENOENT` on a file it was asked to create.

**errno.** The kernel uses POSIX/Linux numbering, and newlib has its own. They
agree across the classic UNIX range — 36 of the kernel's 54 codes are identical
— and diverge above it: `ENOSYS` is 38 to the kernel and 88 to newlib, `ELOOP`
40 against 92, and every socket error differs. Choosing Linux's numbering for
the kernel therefore removed most of a translation table rather than all of it.
A port that assumes "POSIX numbering means no table" reports the wrong failure
for anything above `ERANGE`.

Both tables live in `edos_syscall.h`, written against newlib's own macros rather
than its numbers, so neither can drift from the headers it is compiled with.

## What is not here

- **pthreads.** A separate newlib layer, buildable over the kernel's `clone`
  and futexes. Nothing needs it yet.
- **Sockets.** newlib has none; the BSD calls exist as syscalls and the port
  would add the headers.
- **`link`.** EDOS has symbolic links and no hard links, so it reports `ENOSYS`
  rather than faking one out of a symlink and making `st_nlink` lie.
- **`times`.** The kernel exposes no per-process CPU accounting. It reports
  `ENOSYS`, which is what that errno value was added for.
- **`dlopen` and shared objects**, which need `PT_INTERP` and a dynamic linker
  (stage 3 of `doc/design/dynamic-linking-and-libc.md`).
