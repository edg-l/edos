# What fork did not give the child

Two defects, found together because the second one hid behind the first, and
both are the same shape: an obligation `fork` has to the process it creates
that no test asserted.

## Status

Fixed. Gates: `programs/forktest` (now in `scripts/guest-check`) covers the
registers; `mmaptest` tests 5, 6 and 8 now check the address the child died on
rather than only its exit code, which is what exposes the second.

## Symptom

CI's `guest suites` job was red for a week on `mmaptest`:

```
FAIL test 8 [/var]: child exited 11, expected 0
```

Deterministic in CI, never once locally across ~50 runs of the same case. The
serial log named three user faults in that suite:

```
KILL: PF /bin/mmaptest-fork-55:55 addr=0x1000   (User read)  reject=NoVma
KILL: PF /bin/mmaptest-fork-56:56 addr=0x43d000 (User read)  reject=PastEof
KILL: PF /bin/mmaptest-fork-57:57 addr=0x0      (User write) reject=NoVma
```

Tests 5 and 6 run the *same* expression in their children,
`read_volatile(ptr.add(PAGE))`. One faulted at the mapping's second page, which
is correct; the other at `0x1000`, which is that expression evaluated with
`ptr == 0`. Test 8's child wrote to `0x0`, which is `ptr` itself. So the child's
copy of a pointer its parent had just verified was non-null read as zero, in two
call sites out of three.

`NoVma` was a red herring: at `0x0` and `0x1000` it says only that a null
dereference has no VMA behind it.

## Root cause 1: fork dropped the registers the syscall convention preserves

The SYSCALL stub (`kernel/src/syscalls/mod.rs`) pushes the full register set on
entry and pops `r10, r9, r8, rdx, rsi, rdi` back before `sysretq`. So this
kernel *preserves* the argument registers across a syscall, and the raw stubs
say so: `syscall0` declares only `rax`, `rcx` and `r11` as clobbered. That is
what tells the compiler it may keep a live value in `rdi`/`rsi`/`rdx`/`r8`/
`r9`/`r10` across the `syscall` instruction — and it is inlined, so the
allocator makes that choice inside the function that called `fork`.

`sys_fork` built the child from `CpuContext::new`, which zeroes every GPR, and
then copied back only `rbx`, `rbp` and `r12`–`r15`. The parent therefore
returned from `fork()` with six more registers intact than the child did. Which
call site notices depends entirely on where the register allocator put a live
value, which is why one binary fails at tests 5 and 8 while another passes
everywhere: the same kernel bug, a different allocation.

The fix is to copy every register the stub restores. `rcx` and `r11` are
excluded because SYSCALL itself consumes them for the return address and RFLAGS.

`sys_clone` needs none of this and was left alone: it enters a fresh function at
`func_ptr`, where caller-saved registers carry nothing by the ABI.

## Root cause 2: the kernel could not resolve a COW fault of its own

With the registers fixed, the hardened `mmaptest` failed differently: the child
wrote down the address it was about to touch, and the parent could not read the
file back. `strace` gave `read(4, 0x42f3a0, 7) = -22 EINVAL`, which after
`sys_read` was taught to report the filesystem's own error became `-5 EIO`, the
signature of a failed `try_copy_to_user`.

`fork` marks the whole address space copy-on-write, parent included. A write to
one of those pages is a *protection violation on a present page*, not a missing
page. The ring-0 branch of the page-fault handler serviced only faults without
`PROTECTION_VIOLATION` — demand paging — and fell through to the uaccess fixup
for anything else, which abandons the copy and reports failure. Ring 3 had the
COW branch; ring 0 did not.

So any syscall writing into a page its caller had not touched since forking
failed, and the caller was told its buffer was bad. A `read` into a freshly
allocated buffer is the ordinary way to meet it. The parent of a forked child
was one `Vec::with_capacity` away from it at all times.

## Reasoning rules going forward

- **A syscall's register convention is a contract with both sides of a fork.**
  If the entry stub restores a register, the child must get it too. Changing
  what the stub preserves means changing `sys_fork` in the same commit.
- **The kernel writing to user memory takes the same faults userspace does.**
  Any fault class ring 3 can resolve, ring 0 must resolve too when it is the one
  touching the page: demand fill *and* copy-on-write. `NoFaultGuard` is the only
  exception, and it wants the miss reported rather than serviced.
- **A negative test that asserts only "the child died" asserts almost nothing.**
  Tests 5 and 6 passed in CI on a null dereference while claiming the kernel had
  rejected a past-EOF read. A case built around a fault must pin the address.
- **Do not flatten a filesystem error to `EINVAL`.** `sys_read` did, and it cost
  a build cycle to learn the error was `EIO` and not an argument problem.

## If this reappears

- A child that dies at a small address (`0x0`, `0x1000`, a page multiple)
  while its parent holds a valid pointer is a lost register, not a lost mapping.
  `reject=NoVma` at such an address means "nothing is mapped at 0", nothing more.
- `forktest` names the register that lost its value, so run it first.
- A syscall failing with `EIO` on a buffer that is obviously fine, in a process
  that has forked, is the COW-in-ring-0 shape. `PF-walk` lines in the log and a
  `try_copy_to_user` returning false are the confirmation.
- Reproducing "CI only" was not needed and would have been a detour: the same
  defect was already present locally in a binary that happened not to use those
  registers at those call sites. Build the assertion, not the reproduction.
