# Bug post-mortems

Long-form root-cause writeups for bugs that took effort to diagnose or
that future contributors should recognize if they reappear.

## Naming

`YYYY-MM-DD-<short-slug>.md`, where the date is when the bug was fixed
(or when the writeup was last substantially updated). Example:
`2026-04-13-sched-park-wake-missed-wakeup.md`.

## When to create one

- A bug took more than one iteration to fix, or required structural
  understanding of a subsystem to resolve.
- The root cause contains a design lesson worth preserving (e.g. a
  contract that's easy to get wrong).
- A hang, panic, or data-corruption class that someone could plausibly
  hit again.

## Structure

Match the shape of existing files in this directory. Typical sections:

- **Status** — which commits fixed it, and whether follow-up is needed.
- **Symptoms** — what it looked like from the outside. Include enough
  detail that a future reader can match their hang against yours.
- **Root cause** — the design lesson. Why did this bug exist? What
  invariant was violated? What's the fix in one paragraph?
- **Reasoning rules going forward** — invariants a future contributor
  should preserve. These should also live in CLAUDE.md Critical
  Invariants; repeating them here is a feature, not duplication.
- **If this reappears** — diagnostic flowchart. What to dump, which
  columns to read, how to tell this bug apart from nearby bug classes.
- **Saved artifacts** — paths under `logs/` (gitignored) with serial
  logs, gdb dumps, and thread-state tables from the hang.

Keep writeups scannable; aim for "a future agent with zero context can
diagnose a recurrence in 10 minutes."
