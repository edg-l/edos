//! Key chords the session shell claims before the focused window sees them.
//!
//! The window manager listens on `/dev/kbd`, which is a broadcast: it sees
//! every key, and so does the focused window. A chord the shell acts on —
//! Alt+Tab, Alt+F4 — therefore reaches the application as well, and the
//! application has no way to tell that something else already consumed it.
//!
//! A grab is the shell's statement that a chord is its own. Routing skips the
//! focused window for a claimed chord; the broadcast is untouched, so the
//! claimant still reads it the way it always did.
//!
//! A grab is held per pid and dies with the process, the same as a file
//! descriptor. There is no reclaim path: the only way to lose a grab while
//! alive is to release it.

use alloc::vec::Vec;
use pc_keyboard::KeyCode;

use crate::debug::lock_order::RANK_KEY_GRABS;
use crate::ranked_lock;
use crate::thread::preempt::PreemptSpinlock;

/// Shift is held (either side).
pub const MOD_SHIFT: u32 = 1 << 0;
/// Control is held (either side).
pub const MOD_CTRL: u32 = 1 << 1;
/// Alt is held. AltGr is not Alt: it selects a character, so it is a key the
/// focused window needs rather than a modifier a shell binds against.
pub const MOD_ALT: u32 = 1 << 2;

/// Every bit a caller may set in a grab's modifier mask.
pub const MOD_MASK: u32 = MOD_SHIFT | MOD_CTRL | MOD_ALT;

/// A session binds a handful of chords. The bound exists so a buggy shell
/// cannot grow the table without limit, not to be tight.
const MAX_GRABS: usize = 64;

/// One claimed chord.
struct Grab {
    code: u32,
    mods: u32,
    pid: u64,
}

struct Grabs {
    claimed: Vec<Grab>,
    /// Modifiers held, as the routing thread sees them. Tracked here rather
    /// than in the caller because the decision and the state it is made
    /// against must not drift apart.
    held: u32,
    /// Key codes whose press was withheld. Their release is withheld too:
    /// a window that saw a release with no matching press would leave the key
    /// stuck down in whatever state it keeps.
    swallowed: Vec<u32>,
}

static KEY_GRABS: PreemptSpinlock<Grabs> = PreemptSpinlock::new(Grabs {
    claimed: Vec::new(),
    held: 0,
    swallowed: Vec::new(),
});

/// The modifier bit `code` stands for, if it is a modifier at all.
fn modifier_bit(code: u32) -> Option<u32> {
    if code == KeyCode::LShift as u32 || code == KeyCode::RShift as u32 {
        Some(MOD_SHIFT)
    } else if code == KeyCode::LControl as u32 || code == KeyCode::RControl as u32 {
        Some(MOD_CTRL)
    } else if code == KeyCode::LAlt as u32 {
        Some(MOD_ALT)
    } else {
        None
    }
}

/// Claim `code` with `mods` for `pid`. Returns false if the table is full.
///
/// Claiming a chord that is already claimed by the same process succeeds and
/// changes nothing, so a shell may re-register after a restart without first
/// having to know what it held.
pub fn grab(pid: u64, code: u32, mods: u32) -> bool {
    let mods = mods & MOD_MASK;
    let mut grabs = ranked_lock!(RANK_KEY_GRABS, "window::grab", KEY_GRABS);
    if grabs
        .claimed
        .iter()
        .any(|g| g.code == code && g.mods == mods && g.pid == pid)
    {
        return true;
    }
    if grabs.claimed.len() >= MAX_GRABS {
        return false;
    }
    grabs.claimed.push(Grab { code, mods, pid });
    true
}

/// Release `pid`'s claim on `code`+`mods`. Releasing a chord that was never
/// claimed is not an error: the end state is what the caller asked for.
pub fn ungrab(pid: u64, code: u32, mods: u32) {
    let mods = mods & MOD_MASK;
    ranked_lock!(RANK_KEY_GRABS, "window::ungrab", KEY_GRABS)
        .claimed
        .retain(|g| !(g.code == code && g.mods == mods && g.pid == pid));
}

/// Drop every claim held by `pid`. Called from process teardown, so a later
/// process that happens to reuse the number does not inherit the claims.
pub fn release_pid(pid: u64) {
    ranked_lock!(RANK_KEY_GRABS, "window::grab::release_pid", KEY_GRABS)
        .claimed
        .retain(|g| g.pid != pid);
}

/// Whether the focused window should be spared this key, updating the modifier
/// state the answer is derived from.
///
/// Modifiers themselves are always delivered: a window tracks its own modifier
/// state, and withholding the press that a chord is built from would desync it.
pub fn intercept(code: u32, down: bool) -> bool {
    let mut grabs = ranked_lock!(RANK_KEY_GRABS, "window::grab::intercept", KEY_GRABS);

    if let Some(bit) = modifier_bit(code) {
        if down {
            grabs.held |= bit;
        } else {
            grabs.held &= !bit;
        }
        return false;
    }

    if down {
        let held = grabs.held;
        if grabs
            .claimed
            .iter()
            .any(|g| g.code == code && g.mods == held)
        {
            if !grabs.swallowed.contains(&code) {
                grabs.swallowed.push(code);
            }
            return true;
        }
        return false;
    }

    let before = grabs.swallowed.len();
    grabs.swallowed.retain(|c| *c != code);
    grabs.swallowed.len() != before
}
