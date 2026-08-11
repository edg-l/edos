//! Kernel counters sampled around a run.
//!
//! The deltas are what turn a throughput number into a diagnosis. A "cold"
//! read that reports no block-cache misses never touched the disk; a run with
//! a non-zero watchdog delta stalled on NCQ rather than on the filesystem.

use std::fs::read_to_string;

/// Files sampled, in report order. Every `key: value` or `key=value` pair in
/// them is captured, so a counter added to procfs later shows up here without
/// touching this list.
const SOURCES: &[&str] = &[
    "/proc/block_cache",
    "/proc/ahci_stats",
    "/proc/inflight_stats",
    "/proc/evict_stats",
    "/proc/efs_stats",
];

/// Counters whose value is a current level rather than a running total.
/// Their difference is meaningless, so the report shows the final value.
const LEVELS: &[&str] = &[
    "block_cache.dirty_pages",
    "inflight_stats.current",
    "ahci_stats.timeout_ms",
];

pub struct Counters {
    entries: Vec<(String, u64)>,
}

impl Counters {
    pub fn sample() -> Self {
        let mut entries = Vec::new();
        for path in SOURCES {
            let Ok(text) = read_to_string(path) else {
                continue;
            };
            let source = path.rsplit('/').next().unwrap_or(path);
            for line in text.lines() {
                for (key, value) in parse_pairs(line) {
                    entries.push((format!("{source}.{key}"), value));
                }
            }
        }
        Self { entries }
    }

    /// Counters that moved between `self` and `later`, in sample order.
    pub fn delta(&self, later: &Counters) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (key, after) in &later.entries {
            let before = self
                .entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| *v)
                .unwrap_or(0);
            if LEVELS.contains(&key.as_str()) {
                if *after != 0 {
                    out.push((key.clone(), after.to_string()));
                }
            } else if *after > before {
                out.push((key.clone(), format!("+{}", after - before)));
            }
        }
        out
    }
}

/// One counter's current value, read straight from its procfs file.
///
/// For sampling a single gauge inside a loop, where [`Counters::sample`] would
/// read five files to use one number. A missing file or key reads as 0.
pub fn gauge(path: &str, key: &str) -> u64 {
    let Ok(text) = read_to_string(path) else {
        return 0;
    };
    text.lines()
        .flat_map(parse_pairs)
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or(0)
}

/// Extract every `key: value` / `key=value` pair on one line.
///
/// Both spellings and both spacings occur in procfs: `/proc/block_cache`
/// writes one `key: value` per line, `/proc/ahci_stats` packs several
/// `key=value` onto one.
fn parse_pairs(line: &str) -> Vec<(String, u64)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let field = fields[i];
        if let Some(key) = field.strip_suffix(':') {
            // `key: value` split across two fields.
            if let Some(n) = fields.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                out.push((key.to_string(), n));
            }
            i += 2;
        } else {
            // `key:value` / `key=value` with no space between them.
            if let Some((key, value)) = field.split_once([':', '='])
                && let Ok(n) = value.parse::<u64>()
            {
                out.push((key.to_string(), n));
            }
            i += 1;
        }
    }
    out
}
