//! What the kernel puts on the initial process stack, read back the way a libc
//! reads it (System V x86-64 psABI §3.4.1).
//!
//! `#![no_main]` is what makes this checkable. The runtime's `_start` receives
//! `argv` in a register and hands it to `main`, but a normal Rust `fn main()`
//! never sees it; taking over the C `main` symbol does. Every other part of the
//! initial stack is then found by walking from `argv`, exactly as a `crt1.o`
//! walks it, so a wrong terminator or a wrong order fails here.

#![no_main]

use std::process;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;
const PHDR_SIZE: usize = 56;

/// A vector longer than this is unterminated, not large.
const AUX_SCAN_LIMIT: usize = 64;

fn check(passed: &mut u32, failed: &mut u32, name: &str, ok: bool, detail: String) {
    if ok {
        *passed += 1;
        println!("ok   {name}: {detail}");
    } else {
        *failed += 1;
        println!("FAIL {name}: {detail}");
    }
}

/// Walk `argv` to the auxiliary vector: `argv[argc]` is NULL, `envp` begins one
/// slot later, and the vector begins one slot past `envp`'s NULL.
///
/// # Safety
/// `argv` must be the initial-stack pointer the kernel passed to `_start`.
unsafe fn read_auxv(argc: isize, argv: *const *const u8) -> Vec<(u64, u64)> {
    let mut p = unsafe { argv.offset(argc + 1) };
    while !unsafe { *p }.is_null() {
        p = unsafe { p.add(1) };
    }

    let mut p = unsafe { p.add(1) } as *const u64;
    let mut out = Vec::new();
    loop {
        let a_type = unsafe { *p };
        let a_val = unsafe { *p.add(1) };
        out.push((a_type, a_val));
        if a_type == AT_NULL || out.len() >= AUX_SCAN_LIMIT {
            break;
        }
        p = unsafe { p.add(2) };
    }
    out
}

fn get(aux: &[(u64, u64)], a_type: u64) -> Option<u64> {
    aux.iter().find(|(t, _)| *t == a_type).map(|(_, v)| *v)
}

/// # Safety
/// `ptr` must point to a NUL-terminated string.
unsafe fn cstr(ptr: u64) -> String {
    let base = ptr as *const u8;
    let mut len = 0usize;
    while unsafe { *base.add(len) } != 0 {
        len += 1;
    }
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(base, len) }).into_owned()
}

/// One program header field. Entries are 8-aligned in practice; the unaligned
/// read costs nothing and does not depend on that.
///
/// # Safety
/// `phdr` must point to a mapped table of at least `i + 1` entries.
unsafe fn phdr_u32(phdr: u64, i: usize, off: usize) -> u32 {
    unsafe { ((phdr as *const u8).add(i * PHDR_SIZE + off) as *const u32).read_unaligned() }
}

/// # Safety
/// As [`phdr_u32`].
unsafe fn phdr_u64(phdr: u64, i: usize, off: usize) -> u64 {
    unsafe { ((phdr as *const u8).add(i * PHDR_SIZE + off) as *const u64).read_unaligned() }
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: isize, argv: *const *const u8) -> i32 {
    let mut passed = 0u32;
    let mut failed = 0u32;

    let aux = unsafe { read_auxv(argc, argv) };

    check(
        &mut passed,
        &mut failed,
        "terminated",
        aux.last().map(|(t, _)| *t) == Some(AT_NULL) && aux.len() > 1 && aux.len() < AUX_SCAN_LIMIT,
        format!("{} entries, last type {:?}", aux.len(), aux.last()),
    );

    check(
        &mut passed,
        &mut failed,
        "argc",
        argc as usize == std::env::args().count(),
        format!("argc={argc}, std reports {}", std::env::args().count()),
    );

    check(
        &mut passed,
        &mut failed,
        "AT_PAGESZ",
        get(&aux, AT_PAGESZ) == Some(4096),
        format!("{:?}", get(&aux, AT_PAGESZ)),
    );

    check(
        &mut passed,
        &mut failed,
        "AT_PHENT",
        get(&aux, AT_PHENT) == Some(PHDR_SIZE as u64),
        format!("{:?}", get(&aux, AT_PHENT)),
    );

    check(
        &mut passed,
        &mut failed,
        "AT_BASE",
        get(&aux, AT_BASE) == Some(0),
        format!(
            "{:?}, zero until something honours PT_INTERP",
            get(&aux, AT_BASE)
        ),
    );

    check(
        &mut passed,
        &mut failed,
        "AT_SECURE",
        get(&aux, AT_SECURE) == Some(0),
        format!("{:?}", get(&aux, AT_SECURE)),
    );

    match (get(&aux, AT_PHDR), get(&aux, AT_PHNUM), get(&aux, AT_ENTRY)) {
        (Some(phdr), Some(phnum), Some(entry)) => {
            // PT_PHDR names the table's own address, so the load base is the
            // difference. Finding the ELF magic there is what proves AT_PHDR
            // points at this image's real headers rather than at anything that
            // merely parses.
            let phnum = phnum as usize;
            let mut load_base = None;
            let mut entry_in_load = false;

            for i in 0..phnum.min(AUX_SCAN_LIMIT) {
                let p_type = unsafe { phdr_u32(phdr, i, 0) };
                let p_vaddr = unsafe { phdr_u64(phdr, i, 16) };
                if p_type == PT_PHDR {
                    load_base = Some(phdr - p_vaddr);
                }
            }

            check(
                &mut passed,
                &mut failed,
                "AT_PHDR/PT_PHDR",
                load_base.is_some(),
                format!("phdr={phdr:#x}, phnum={phnum}, load_base={load_base:x?}"),
            );

            if let Some(base) = load_base {
                let magic = unsafe { std::slice::from_raw_parts(base as *const u8, 4) };
                check(
                    &mut passed,
                    &mut failed,
                    "AT_PHDR/elf-magic",
                    magic == b"\x7fELF",
                    format!("{magic:x?} at load base {base:#x}"),
                );

                for i in 0..phnum.min(AUX_SCAN_LIMIT) {
                    let p_type = unsafe { phdr_u32(phdr, i, 0) };
                    let p_vaddr = unsafe { phdr_u64(phdr, i, 16) };
                    let p_memsz = unsafe { phdr_u64(phdr, i, 40) };
                    if p_type == PT_LOAD
                        && entry >= base + p_vaddr
                        && entry < base + p_vaddr + p_memsz
                    {
                        entry_in_load = true;
                    }
                }
                check(
                    &mut passed,
                    &mut failed,
                    "AT_ENTRY",
                    entry_in_load,
                    format!("{entry:#x} inside a PT_LOAD of the image at {base:#x}"),
                );
            }
        }
        other => check(
            &mut passed,
            &mut failed,
            "AT_PHDR",
            false,
            format!("missing one of PHDR/PHNUM/ENTRY: {other:?}"),
        ),
    }

    match get(&aux, AT_RANDOM) {
        Some(ptr) => {
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, 16) };
            check(
                &mut passed,
                &mut failed,
                "AT_RANDOM",
                bytes.iter().any(|&b| b != 0),
                format!("16 bytes at {ptr:#x}, first four {:x?}", &bytes[..4]),
            );
        }
        None => check(
            &mut passed,
            &mut failed,
            "AT_RANDOM",
            false,
            "absent; musl seeds its stack guard from it".to_string(),
        ),
    }

    match get(&aux, AT_EXECFN) {
        Some(ptr) => {
            let path = unsafe { cstr(ptr) };
            check(
                &mut passed,
                &mut failed,
                "AT_EXECFN",
                path.ends_with("auxvtest"),
                format!("{path:?}"),
            );
        }
        None => check(
            &mut passed,
            &mut failed,
            "AT_EXECFN",
            false,
            "absent".into(),
        ),
    }

    println!("auxvtest: {passed} passed, {failed} failed");
    if failed > 0 { process::exit(1) } else { 0 }
}
