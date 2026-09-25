# `poll` never reported a pipe whose writer had gone

## Status

Fixed in `503e0d6e`, both halves. `PollState::matches` (now in
`libs/syscall-abi/src/lib.rs`) reports error, hang-up and invalid
unconditionally. `Pipe::poll_state` (`kernel/src/thread/pipe.rs`) reports a
drained, writerless pipe as readable as well as hung up.

## Symptoms

`echo hi | nc 10.0.2.2 9099` sent its line and then hung forever. `strace`:

```
read(0, "hi\n", 4096) = 3
write(5, "hi\n", 3) = 3
poll(0x428368, 2, -1) <unfinished ...>
```

`echo` had exited, so a `read` on stdin would have returned 0 at once, but
`poll` slept.

## Root cause

Two defects; either alone unhangs `nc`, and both are right independently.

1. `PollState::matches` required the caller to ask for hang-up before it would
   report one. POSIX makes `POLLERR`, `POLLHUP` and `POLLNVAL` output-only:
   reported whether or not `events` lists them, so a reader waiting for data
   cannot wait forever on a descriptor whose peer has gone. `matches` now
   consults the interests only for readable and writable.
2. `Pipe::poll_state` set only `hangup` on a drained pipe with no writer. A
   read there returns end of file immediately, which is readable; both PTY
   sides already reported it that way.

Nothing had hit it before because every existing poll loop sat on a PTY.

## Reasoning rules going forward

- Error, hang-up and invalid are always reported. A poll implementation that
  filters them by the caller's interests can hang a reader forever.
- A descriptor whose `read` would return immediately is readable, end of file
  included.
- A program polling a descriptor must read it unbuffered. `nc` reads stdin
  with the raw `read` syscall, because a buffered `Stdin` could hold data
  `poll` no longer reports.

## If this reappears

1. A poll loop on a non-PTY descriptor that never notices end of input.
2. `strace` the program: a `poll` that never returns while a `read` on one of
   its descriptors would return 0.
3. Check the descriptor type's `poll_state` for the end-of-file case, then
   `PollState::matches` for interest filtering.
