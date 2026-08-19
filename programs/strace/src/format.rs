//! Turning a pair of trace records back into the call that produced them.

use edos_lib::trace::{ArgKind, SyscallTable, TRACE_STR_FAULT, TRACE_STR_TRUNCATED, TraceRecord};

/// `dirfd` value meaning "resolve against the working directory".
const AT_FDCWD: i64 = -100;

/// `name(args)`, with the return record folded in where it carries something
/// an argument needs — the contents of a buffer the call filled.
fn head(
    table: &SyscallTable,
    enter: &TraceRecord,
    exit: Option<&TraceRecord>,
    max_str: usize,
) -> String {
    format!(
        "{}({})",
        table.name_of(enter.nr),
        args(table, enter, exit, max_str)
    )
}

/// A completed call.
pub fn format_call(
    table: &SyscallTable,
    enter: &TraceRecord,
    exit: &TraceRecord,
    max_str: usize,
) -> String {
    format!(
        "{} = {}",
        head(table, enter, Some(exit), max_str),
        result(table, exit)
    )
}

/// A call still in flight.
pub fn format_unfinished(table: &SyscallTable, enter: &TraceRecord, max_str: usize) -> String {
    format!("{} <unfinished ...>", head(table, enter, None, max_str))
}

/// A call the thread died inside: `exit` is the obvious one, but so is any
/// call interrupted by a kill.
pub fn format_abandoned(table: &SyscallTable, enter: &TraceRecord, max_str: usize) -> String {
    format!("{} = ?", head(table, enter, None, max_str))
}

/// The second half of a call that was reported unfinished.
pub fn format_resumed(table: &SyscallTable, exit: &TraceRecord) -> String {
    format!(
        "<... {} resumed> = {}",
        table.name_of(exit.nr),
        result(table, exit)
    )
}

fn result(table: &SyscallTable, exit: &TraceRecord) -> String {
    let signed = exit.ret as i64;
    let mut out = if signed < 0 {
        format!("{signed}")
    } else {
        format!("{}", exit.ret)
    };

    // Errno is only meaningful on a failure; a successful call leaves whatever
    // the previous one set.
    if signed < 0
        && exit.errno != 0
        && let Some(name) = table.errno_name(exit.errno)
    {
        out.push(' ');
        out.push_str(name);
    }
    out
}

fn args(
    table: &SyscallTable,
    enter: &TraceRecord,
    exit: Option<&TraceRecord>,
    max_str: usize,
) -> String {
    let Some(info) = table.calls.get(&enter.nr) else {
        // An unknown number: show every register rather than guess an arity.
        return enter
            .args
            .iter()
            .map(|a| format!("0x{a:x}"))
            .collect::<Vec<_>>()
            .join(", ");
    };

    let mut parts = Vec::with_capacity(info.args.len());
    let mut slot = 0;

    for (i, kind) in info.args.iter().enumerate() {
        // The kind string comes from /proc/syscalls at runtime and nothing
        // bounds it to the six argument registers.
        let Some(&raw) = enter.args.get(i) else { break };
        parts.push(match kind {
            ArgKind::Str => {
                let text = captured(enter, slot, max_str, None);
                slot += 1;
                text.unwrap_or_else(|| pointer(raw))
            }
            ArgKind::StrLen => {
                let declared = enter.args.get(i + 1).copied().unwrap_or(0) as usize;
                let text = captured(enter, slot, max_str, Some(declared));
                slot += 1;
                text.unwrap_or_else(|| pointer(raw))
            }
            ArgKind::Out => match exit {
                // Sized by the return value, so a failed call has nothing to
                // show and falls back to the address userspace passed.
                Some(exit) if (exit.ret as i64) > 0 => {
                    captured(exit, 0, max_str, Some(exit.ret as usize))
                        .unwrap_or_else(|| pointer(raw))
                }
                _ => pointer(raw),
            },
            ArgKind::Fd => {
                if raw as i64 == AT_FDCWD {
                    "AT_FDCWD".to_string()
                } else {
                    format!("{}", raw as i64)
                }
            }
            ArgKind::Ptr | ArgKind::Buf => pointer(raw),
            ArgKind::Hex => format!("0x{raw:x}"),
            ArgKind::Int => format!("{}", raw as i64),
            ArgKind::Len => format!("{raw}"),
        });
    }

    parts.join(", ")
}

fn pointer(value: u64) -> String {
    if value == 0 {
        "NULL".to_string()
    } else {
        format!("0x{value:x}")
    }
}

/// The `slot`-th captured buffer, quoted and escaped.
///
/// `declared` is what the caller said the length was, so a buffer longer than
/// the record can hold is marked truncated rather than silently shortened.
fn captured(
    rec: &TraceRecord,
    slot: usize,
    max_str: usize,
    declared: Option<usize>,
) -> Option<String> {
    match rec.str_lens.get(slot).copied()? {
        // The address was bad. The caller falls back to showing it raw.
        TRACE_STR_FAULT => return None,
        // The address was fine; an earlier argument filled the record.
        TRACE_STR_TRUNCATED => return Some("...".to_string()),
        _ => {}
    }
    let bytes = rec.captured_str(slot)?;
    let shown = bytes.len().min(max_str);
    let mut out = String::with_capacity(shown + 2);
    out.push('"');
    for &byte in &bytes[..shown] {
        escape_into(&mut out, byte);
    }
    out.push('"');
    if shown < declared.unwrap_or(bytes.len()) {
        out.push_str("...");
    }
    Some(out)
}

fn escape_into(out: &mut String, byte: u8) {
    match byte {
        b'\n' => out.push_str("\\n"),
        b'\r' => out.push_str("\\r"),
        b'\t' => out.push_str("\\t"),
        b'"' => out.push_str("\\\""),
        b'\\' => out.push_str("\\\\"),
        0x20..=0x7e => out.push(byte as char),
        _ => out.push_str(&format!("\\x{byte:02x}")),
    }
}
