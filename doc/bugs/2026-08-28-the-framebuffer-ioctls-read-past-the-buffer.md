# The framebuffer ioctls read past the ioctl buffer (2026-08-28)

## Status

**Fixed** in `12a3cd4e` (the length reaches the device) and gated by
`1020f3ed` (`programs/fbtest`, the twentieth suite in `scripts/guest-check`).
`kernel/src/graphics/` now carries `#[deny(clippy::undocumented_unsafe_blocks)]`
like the seven modules before it; it could not, honestly, before the fix,
because three of its blocks were unsound and a `// SAFETY:` on one of them
would have been a false claim.

Found while writing those comments, not from a crash. No guest had tripped it:
the only caller, `programs/edos_render/src/graphics.rs`, passes the right
length everywhere.

## Symptoms

None observed in a running guest. What it would look like:

- Kernel heap contents drawn on the display, then a page fault in kernel mode
  from `FB_IOCTL_DRAW`, with the faulting address well past a small heap
  allocation.
- Silent heap corruption 12 or 40 bytes past an allocation after
  `FB_IOCTL_SCREEN_INFO` or `FB_IOCTL_MMAP_INFO`, which *write* their answer
  through the pointer.

Any process that can open `/dev/fb0` could do either with a single `ioctl`.

## Root cause

`sys_ioctl` (`kernel/src/syscalls/ioctl/mod.rs`) allocated `vec![0u8; arg_len]`
and copied `arg_len` bytes in from the user pointer. `arg_len` is a syscall
argument, so **userspace chose it**. The buffer pointer, and nothing else, then
reached the device: `DevFsDevice::ioctl(&self, request, arg)` had no length
parameter, and neither did `FileSystem::ioctl` or `fs::api::ioctl` behind it.
Every handler in `graphics/framebuffer.rs` read its header out of that buffer
having checked only `arg != 0`.

`FB_IOCTL_DRAW` was the worst shape: it built a slice of `header.pixel_count`
u32s *after* the header, with `pixel_count` checked against `width * height`
and against nothing else. `arg_len = size_of::<FramebufferDraw>()` with
`width = height = 4096` made the kernel form a 64 MB slice over a 32-byte heap
allocation and blit it to the screen. `FB_IOCTL_SET_CURSOR` was identical.

The sentence that hid it was already in the file: *"arg points to kernel-copied
ioctl buffer (safe to read)"*. Copying a buffer into the kernel says nothing
about how far into it a reader may go; that is what the length is for, and the
length was thrown away one frame above.

## The fix

At the trait, not the call site. `arg_len` is known in `sys_ioctl`, so it is
threaded down beside the pointer: `DevFsDevice::ioctl(&self, request, arg,
arg_len)`, `FileSystem::ioctl(&self, path, request, arg, arg_len)`, and the
`fs::api` and `fs::vfs` hops between them. The doc comment on
`DevFsDevice::ioctl` states the contract and says why the device is the only
layer that can enforce it: neither the syscall layer nor devfs knows the shape
the request names.

Three details that are not obvious from the diff:

- On the no-buffer path `arg` is a scalar and `arg_len` is passed as **0**,
  whatever the caller said, so a device cannot be tricked into treating a raw
  user pointer as a kernel buffer.
- The buffer is now `Vec<u64>` of `arg_len.div_ceil(8)`, not `Vec<u8>`.
  `Vec<u8>` promises alignment 1, and devices read `#[repr(C)]` structs and
  `u32` slices straight out of it; `ptr::read` and `slice::from_raw_parts` both
  require alignment, so every one of those was resting on an allocator
  implementation detail (size-class allocations happen to be aligned to their
  class). The rounding only ever adds slack past the end and nothing is told
  about it, so the byte length stays `arg_len`.
- `graphics/framebuffer.rs` grew a private `IoctlBuf { ptr, len }` with three
  accessors: `header::<T>()` and `out::<T>()` (both check
  `len >= size_of::<T>()`), and `tail_u32::<T>(count)` (checks
  `size_of::<T>() + count * 4 <= len` with checked arithmetic, so a huge count
  cannot wrap into a small product). Every arm goes through them, so a new
  request cannot forget the check by being written in the older style.
  `IoctlBuf::new` is the one `unsafe fn`, and it takes a null `arg` as an empty
  buffer rather than refusing it, which is what lets the scalar requests
  (`FB_IOCTL_RENDER`, `FB_IOCTL_FLIP`, `FB_IOCTL_FLIP_WAIT`) share it.

`hda` and the block device take `_arg_len` today because both read `arg` as a
scalar only. The parameter is there so the next variable-length request has the
length in hand rather than rediscovering this.

## Reasoning rules going forward

- **A buffer copied in from userspace is a pointer *and* a length, and the two
  never separate.** A signature that carries only the pointer has moved the
  bounds check to a layer that cannot perform it.
- **A length userspace chose bounds the copy, not the read.** `arg_len` is
  honest about how much was copied in; a handler that reads a header and then a
  tail must check both against it.
- **Alignment is not a favour the allocator does you.** `Vec<u8>` is alignment
  1; reading a `#[repr(C)]` struct or a `u32` slice out of one is UB even when
  it works.
- A safety comment that restates where a pointer came from is not a safety
  argument. It has to name the bound.

## If this reappears

`make guest-check` runs `programs/fbtest` in a real guest. It opens `/dev/fb0`
and asks each request for more bytes than it was given: the two that write
their answer through the pointer with room for only part of it, the four
fixed-size headers with a buffer too small to hold one, and `FB_IOCTL_DRAW` /
`FB_IOCTL_SET_CURSOR` with a header that is internally consistent about an 8x8
rectangle in a buffer carrying the header and not one pixel, then the same one
pixel short. Every case must come back an error.

Its first case is the one that makes the rest mean anything: a correctly sized
`FB_IOCTL_SCREEN_INFO` that must *succeed* and answer a non-zero screen.
Without it a suite of refusals passes just as well against a machine with no
framebuffer, or against a device that has started refusing everything.

To check a *different* device for the same shape, read its `ioctl` arm and ask
what bounds each read: if the answer is a field of the struct it just read out
of the buffer, that is this bug.
