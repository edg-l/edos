# Dynamic linking, and whether a libc can be ported

Two questions, answered against the code as it is: what it would take to support
`PT_INTERP` and a dynamic linker, and whether a C library can be ported so that
software written outside this repository can be compiled for EDOS.

The short answer is at the end of section 4. Sections 1–3 are the evidence.

## 1. What the process ABI is today

Everything below is read from the tree, not from what the system resembles.

### 1.1 The loader

`kernel/src/loader/mod.rs` recognises exactly two program-header types:
`PT_LOAD` (line 355) and `PT_TLS` (line 480). There is no constant for
`PT_INTERP`, `PT_DYNAMIC` or `PT_GNU_RELRO`, and an unknown `p_type` is skipped
silently. `e_type` must be `ET_EXEC` (load base 0) or `ET_DYN` (load base
`0x400000`); anything else is rejected (lines 313–317).

Relocations are read from **`SHT_RELA` section headers** (line 509), not from
`PT_DYNAMIC`'s `DT_RELA`. Only `R_X86_64_RELATIVE` is applied; `JUMP_SLOT` and
`GLOB_DAT` return `UnsupportedRelocation` (line 558), as does an `SHT_REL`
section (line 566) and a `RELATIVE` entry carrying a non-zero symbol index. The
table is built once per image and applied lazily per page
(`kernel/src/loader/reloc.rs`, design in `doc/design/lazy-elf-reloc.md`).

Two consequences worth stating plainly. Reading relocations from section headers
rather than the dynamic segment means a **stripped shared object cannot be read
at all** — `.so` files are routinely shipped with section headers removed, and
`PT_DYNAMIC` is the only structure guaranteed to survive. And the loader assumes
**one image per address space**: `LoadedInfo` carries a single `load_base` and a
single `Arc<RelocTable>`.

### 1.2 The entry convention

`setup_user_stack` (`kernel/src/thread/mod.rs:89`) builds the SysV prefix. The
argument and environment strings go at the top of the stack, then, descending:
the `envp` NULL, the `envp` pointers, the `argv` NULL, the `argv` pointers, and
`argc` last, so `argc` sits at `[rsp]` (`mod.rs:207`) exactly as the psABI wants.

Three things separate that from the SysV process entry ABI (psABI §3.4.1).

**There is no auxiliary vector.** The stack ends after the `envp` NULL: no
`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_ENTRY`, `AT_BASE`, `AT_PAGESZ`,
`AT_RANDOM`, `AT_SECURE`, `AT_EXECFN`.

**The registers are the live channel, not the stack.**
`kernel/src/thread/thread.rs:999` sets `rdi = argc`, `rsi = argv`, `rdx = envp`,
and the entry point is entered as an ordinary SysV **function** — the fork's is
`extern "C" fn _start(argc, argv, envp)` in
`library/std/src/sys/pal/edos/start.rs`. Nothing reads the stack copy today.
Note that `rdx` is where the psABI puts a function pointer for the process to
register with `atexit`: glibc's `_start` moves it to `%r9` and calls it as
`rtld_fini`, so a glibc binary entered this way would call the `envp` array.

**The alignment is the function-call convention.** `mod.rs:141-166` pads to
`rsp % 16 == 8`, the state a callee sees after `call` pushes a return address,
because `_start` is treated as a function. Process entry wants `rsp % 16 == 0`
with `argc` at `[rsp]`. Both glibc's and musl's `_start` begin by masking `%rsp`
down, so in practice they tolerate it, but the padding rule has to invert to be
conforming.

The missing auxv is what blocks both questions. A dynamic linker learns where the
main image's program headers are, and where it was itself loaded, from `AT_PHDR`
and `AT_BASE`; there is no other channel. Every libc's `crt1.o`/`_start` reads
`argc` off the stack, walks past the `envp` NULL, and hands the auxv to
`__libc_start_main`; musl additionally requires `AT_RANDOM` for its stack guard.

### 1.3 TLS is owned by the kernel

`allocate_tls_region` (`kernel/src/thread/thread.rs:661`) parses the `PT_TLS`
template, allocates the block from a per-address-space slot counter at a fixed
128 KiB stride below the stack (`TLS_REGION_STRIDE`, line 290), zeroes it, copies
the init image, writes the TCB self-pointer, and returns an `fs_base`. The
scheduler restores FS on every switch (`kernel/src/thread/scheduler.rs:805`).

There is **no `arch_prctl(ARCH_SET_FS)` or `set_thread_area`**: userspace cannot
set its own FS base. The model supported is static TLS only — local-exec and
initial-exec. There is no DTV, no `__tls_get_addr`, no TLSDESC, so the
general-dynamic and local-dynamic models do not exist.

### 1.4 The syscall surface

116 numbers, all in `kernel/src/syscalls/table.rs`. Entry is `SYSCALL`/`SYSRET`
(`syscalls/mod.rs:196`).

Present and directly useful: `mmap` with `MAP_FIXED`, `MAP_PRIVATE`,
`MAP_SHARED`, `MAP_ANONYMOUS`, file backing with an offset and `PROT_EXEC`
honoured (`syscalls/memory.rs`); `futex_wait`/`futex_wake`; `clone`; `openat`
and the whole `*at` family; `poll`; `readv`/`writev`; `pread`/`pwrite`; the BSD
socket calls; `sigaction`, `sigprocmask`, `sigreturn`, `kill`.

Absent, and each is load-bearing for the questions here:

| Missing | Who needs it |
| --- | --- |
| `mprotect` | RELRO; a linker protecting its own GOT; any JIT; `dlopen` |
| `brk`/`sbrk` | newlib's allocator (can be faked over `mmap` in a port's stub) |
| `arch_prctl`-equivalent | any libc that wants to own TLS |
| `set_tid_address` | `pthread_join`'s usual exit notification |
| `sigaltstack`, `siginfo` | stack-overflow handling; anything reading `si_addr` |
| termios (`TCGETS`/`TCSETS`) | every terminal program; ncurses |
| a `clockid` on `clock_gettime` | `CLOCK_MONOTONIC` versus wall time (table.rs:119 takes one buffer) |

### 1.5 Errors

A failing syscall returns `u64::MAX` and the caller then issues a **second
syscall**, `SYS_ERRNO`, to find out why. `Errno` (`syscalls/mod.rs:1294`) is a
dense EDOS-private enum of 26 values in its own numbering, beginning
`Clear = 0, EINVAL = 1, ENOMEM = 2`.

Both halves are a problem for a libc. Every C library expects the Linux
convention of a negative errno in the return register, so a shim must translate;
the translation costs an extra kernel entry on every failure, and it is not
atomic — a signal handler that makes a syscall between the failed call and
`SYS_ERRNO` overwrites the value. And 26 errno values against POSIX's ~130 means
`ENOSYS`, `ERANGE`, `EDOM`, `EOVERFLOW`, `ENOTSUP`, `ETIMEDOUT` and the rest have
nothing to map to. `ERANGE`/`EDOM` alone are required by C99 `math.h`.

### 1.6 Threads and signals

`sys_clone` (`syscalls/mod.rs:2608`) takes `(func_ptr, arg, flags, child_stack)`
and **ignores `flags`** — it is a thread-spawn primitive, not Linux's `clone`.
That is closer to what `pthread_create` wants than Linux's interface is, so it is
less of an obstacle than it first looks; the gap is exit notification, which
`set_tid_address` provides on Linux and which futexes could provide here.

`SYS_SIGACTION` takes three scalars (table.rs:125), not a `struct sigaction`.
There is no `sigaltstack` and no `siginfo_t`.

### 1.7 The terminal

The pty exposes EDOS-private ioctls — `PTY_IOCTL_SET_RAW`, `SET_CANONICAL`,
`GET_MODE`, `SET_WINSIZE`, `GET_WINSIZE` (`kernel/src/thread/pty.rs:52`). There
is no `struct termios` anywhere in the tree.

## 2. Question 1: `PT_INTERP` and a dynamic linker

### 2.1 What has to change in the kernel

1. **The initial process stack.** `setup_user_stack` already lays down
   `argc`, `argv[]`, NULL, `envp[]`, NULL; what it does not lay down is the
   auxiliary vector, so append one after the `envp` NULL carrying at minimum
   `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_ENTRY`, `AT_BASE`, `AT_PAGESZ`,
   `AT_RANDOM`, `AT_SECURE`, `AT_EXECFN`, `AT_NULL`, and reserve room in the
   string area for `AT_RANDOM`'s 16 bytes and `AT_EXECFN`'s path. This can be
   **additive**: keep `rdi`/`rsi`/`rdx` set as they are, so every existing binary
   and the `edos_rt` `_start` keep working unchanged, and let a new binary read
   the stack instead. That matters because changing `_start` means changing the
   Rust fork. The entry alignment (§1.2) is the part that cannot stay additive
   forever — an interpreter's own `_start` is entitled to a 16-aligned `%rsp`.
2. **Two images in one address space.** Load the interpreter named by
   `PT_INTERP` as a second `ET_DYN` image at its own base, and enter *its*
   `e_entry` with the main image mapped but unrelocated. `LoadedInfo` grows from
   one `load_base`/`RelocTable` to one per image.
3. **`mprotect`.** Without it the linker cannot apply RELRO and cannot protect
   its own GOT after binding.
4. **TLS ownership moves to userspace.** This is the largest change and the one
   that cannot be made additive. With shared libraries the static TLS block is
   the executable's `PT_TLS` *plus* every `DT_NEEDED` library's, laid out by the
   linker, which must then allocate the DTV and set FS itself. That needs an
   `arch_prctl`-equivalent syscall, and `allocate_tls_region` must become
   optional rather than something every user thread gets.

Note what does **not** have to change: the kernel needs *less* relocation
machinery, not more. A dynamic executable's relocations are the interpreter's
job. The kernel only has to relocate the interpreter itself, which is a
static-PIE carrying nothing but `R_X86_64_RELATIVE` — exactly what the existing
lazy `RelocTable` already handles. `doc/design/lazy-elf-reloc.md`'s limitations
section is right that `JUMP_SLOT`/`GLOB_DAT` should stay unimplemented in the
kernel; they belong in the linker.

The `SHT_RELA`-versus-`DT_RELA` gap in section 1.1 does have to be fixed, but in
the interpreter, not the kernel.

### 2.2 What the interpreter would be

A static-PIE Rust binary in this tree, `programs/edos-ld`, roughly 2000–3000
lines: ELF and `PT_DYNAMIC` parsing, `DT_NEEDED` resolution with a search path,
GNU hash symbol lookup, relocation application (`RELATIVE`, `GLOB_DAT`,
`JUMP_SLOT`, `64`, `COPY`, `TPOFF64`, `DTPMOD64`/`DTPOFF64`, `IRELATIVE`), static
TLS layout plus DTV allocation, and the `dlopen`/`dlsym`/`dlclose` entry points.

**Do eager binding only to start with.** `DT_BIND_NOW`/`-z now` resolves every
`JUMP_SLOT` at load time and removes the need for a `_dl_runtime_resolve`
assembly trampoline and a writable GOT entirely. Lazy PLT binding is a latency
optimisation that can come later, if it is ever measured to matter.

Two things are free here. File-backed `mmap` with `PROT_EXEC` already exists, so
the linker maps segments with the same machinery the kernel uses, and the page
cache is per-inode and shared across processes, so a library's `.text` is shared
between every process that maps it without any new mechanism.

### 2.3 What it would actually buy

Honestly: not much yet.

`filesystem/bin` is 32 MB stripped (it was 117 MB before `f61abe0` stripped it),
and the live-root image's size is set by a 64 MiB floor in the `GNUmakefile`, not
by the binaries. Sharing one copy of Rust's `std` across the whole tree would
shrink it and cut per-process resident memory, but neither is a felt problem
today.

No program in the tree wants `dlopen`. Nothing is blocked on it.

The real reason to want it is that it is a *prerequisite for other people's
software*, which is question 2 — and question 2 does not actually need it, since
static linking is available.

## 3. Question 2: can a libc be ported?

### 3.1 musl

Not the right target. musl has no porting layer: it is a Linux libc, and its
internals assume Linux syscall numbering, Linux struct layouts, and a
negative-errno return. Bringing it up means writing a new architecture/OS backend
inside a codebase that has no concept of one, and then growing the EDOS kernel a
Linux-shaped `struct termios`, `struct sigaction` with a restorer,
`struct stat`, `struct dirent`, `clockid_t`, `sigaltstack`, `set_tid_address`,
`exit_group` and `tkill` to satisfy it.

That is not a port, it is a decision to make EDOS a Linux clone at the ABI level.
It may be the right decision one day. It should not be made by accident as the
side effect of wanting a C compiler to work.

### 3.2 picolibc

Cheapest to bring up and gives the least. Its syscall surface is roughly
`read`, `write`, `lseek`, `close`, `sbrk`, `exit`, `kill`, `getpid`, all of which
exist here or are trivial. But it targets embedded systems: no sockets, no
`fork`/`exec` model to speak of, and a stdio deliberately smaller than C99.

It would let a self-contained C program that only does stdio compile and run. It
would not let anything from a distribution's source tree compile.

### 3.3 newlib

The honest cheapest path to real C software, and the one designed for exactly
this situation. newlib's OS dependency is a documented ~19-function stub layer
(`libgloss`): `_read`, `_write`, `_open`, `_close`, `_lseek`, `_fstat`, `_stat`,
`_isatty`, `_sbrk`, `_exit`, `_kill`, `_getpid`, `_fork`, `_execve`, `_wait`,
`_link`, `_unlink`, `_times`, `gettimeofday`.

Against section 1.4, **every one of those maps onto a syscall that already
exists**, with two exceptions: `_sbrk`, which the stub can implement over
`mmap` (the standard approach for an mmap-only kernel), and `_times`, which needs
per-process CPU accounting the kernel does not expose — it can return -1 with
`ENOSYS`, which is what the missing errno value in section 1.5 is for.

What newlib does not give: pthreads (a separate layer, buildable on the existing
`clone` + futexes) and sockets (newlib has none; the port adds them, and the BSD
calls already exist as syscalls). Neither is needed for the first C program.

### 3.4 A shim over `edos_rt`

Writing our own libc in Rust, exporting the C symbols. Full control and no ABI
drift toward Linux, which is the one real advantage.

Against it: this is not a shim, it is a libc. The long tail — locale, `printf`
positional arguments and rounding, stdio buffering semantics, `setjmp`/`longjmp`,
`strtod` correctness, `math.h` accuracy — is where the years go, and getting any
of it subtly wrong produces third-party software that builds and then misbehaves.

The useful observation is that the EDOS half of a newlib port *is* this shim,
scoped to 19 functions with a specification, and with a tested libc on top of it.

## 4. Recommendation

**Neither first.** Both questions are blocked behind the same three small ABI
changes, and each of those three is worth doing on its own merits:

1. **Finish the SysV initial process stack with a real auxv**, additively, keeping
   the register convention. Nothing else in either project can start without it,
   and it is the only item here that also has to be understood by the Rust fork
   later.
2. **Return `-errno` from the syscall entry** and widen `Errno` to POSIX
   numbering. This removes a syscall from every failure path, removes a real
   (if rare) race against signal handlers, and is the difference between a libc
   port translating a table and a libc port guessing.
3. **Add `mprotect`.** One syscall, needed by the linker, by `dlopen`, by RELRO,
   and by anything that ever wants to JIT.

**Then the libc, and specifically newlib — not the dynamic linker.** The reasons,
in order:

- Dynamic linking buys image size and `dlopen`. Neither is a problem the system
  actually has: the boot image is 32 MB of binaries under a 64 MiB floor, and no
  program in the tree wants a plugin.
- A libc buys the thing that *is* blocked: the `packages/` tree carrying software
  that was not written for this repository. Today that tree can only ever hold
  Rust rebuilt from source.
- **A libc does not need the dynamic linker.** Statically linking against a C
  library is the normal case and the one that works; `PT_INTERP` is a second
  step, not a prerequisite. Doing the linker first would be paying the harder
  cost for the smaller benefit.
- The linker's hardest piece — moving TLS ownership out of the kernel — is not
  needed by a static libc at all, since the kernel's existing `PT_TLS` handling
  already implements exactly the static TLS model a static binary uses.

Staged, with each stage independently landable:

- **Stage 0** — the three ABI changes above.
- **Stage 1** — `libs/libgloss-edos`, the 19-function newlib stub layer, plus a
  build recipe. Done when a C `hello world` compiles and runs in the guest.
- **Stage 2** — termios and sockets in the port, and the first real third-party C
  program in `packages/`.
- **Stage 3** — `PT_INTERP`, userspace TLS ownership, and `programs/edos-ld` with
  eager binding only.

### The price, stated deliberately

A libc port pulls the kernel's ABI toward Linux's. Every structure it makes us
grow — `termios`, `sigaction`, `stat`, `dirent`, `clockid_t` — is one we will not
be able to change afterwards, because software will depend on the layout. That is
a real cost and the reason to prefer newlib over musl: newlib's stub layer lets
us choose which of those we adopt and when, while musl demands all of them at
once, up front, in Linux's exact shape.
