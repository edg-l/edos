//! A plain-text rendering of a parsed document.
//!
//! This is the stage-1 output and it stays useful after the window exists: it
//! is the only rendering that can be asserted on from a headless run.

use std::fmt::Write as _;

use crate::doc::{Block, BlockKind, Document};

/// Render `document` wrapped to `width` columns.
///
/// `links` appends a reference list, each entry numbered by the marker left
/// beside the link text, the way a text browser's `-dump` does.
pub fn render(document: &Document, width: usize, links: bool) -> String {
    let width = width.max(20);
    let mut out = String::new();
    let mut refs: Vec<String> = Vec::new();

    if !document.title.is_empty() {
        let _ = writeln!(out, "{}\n", document.title);
    }

    for block in &document.blocks {
        match block.kind {
            BlockKind::Rule => {
                let _ = writeln!(out, "{}\n", "-".repeat(width.min(60)));
                continue;
            }
            BlockKind::Pre => {
                for line in block.text().lines() {
                    let _ = writeln!(out, "    {}", line);
                }
                out.push('\n');
                continue;
            }
            _ => {}
        }

        let (prefix, indent) = lead(block);
        let body = inline(block, links, &mut refs);
        let hang = " ".repeat(prefix.chars().count());
        let avail = width
            .saturating_sub(prefix.chars().count() + indent.len())
            .max(20);

        for (i, line) in wrap(&body, avail).into_iter().enumerate() {
            let lead = if i == 0 {
                prefix.as_str()
            } else {
                hang.as_str()
            };
            let _ = writeln!(out, "{}{}{}", indent, lead, line);
        }
        out.push('\n');
    }

    if links && !refs.is_empty() {
        out.push_str("References\n\n");
        for (i, target) in refs.iter().enumerate() {
            let _ = writeln!(out, "  [{}] {}", i + 1, target);
        }
    }
    out
}

/// The marker a block wears and the indent it sits at.
fn lead(block: &Block) -> (String, String) {
    match block.kind {
        BlockKind::Heading(level) => (format!("{} ", "#".repeat(level as usize)), String::new()),
        BlockKind::ListItem { depth, marker } => (marker.ascii(), "  ".repeat(depth + 1)),
        BlockKind::Quote => ("> ".to_string(), String::new()),
        _ => (String::new(), String::new()),
    }
}

/// The block's text with link markers folded in, collecting the targets.
fn inline(block: &Block, links: bool, refs: &mut Vec<String>) -> String {
    let mut out = String::new();
    for run in &block.runs {
        out.push_str(&run.text);
        if let Some(target) = &run.link
            && links
        {
            // One number per link, reused when the same target appears twice
            // in a row, which is what a linked image beside its caption gives.
            let index = match refs.iter().position(|r| r == target) {
                Some(index) => index,
                None => {
                    refs.push(target.clone());
                    refs.len() - 1
                }
            };
            let _ = write!(out, "[{}]", index + 1);
        }
    }
    out
}

/// Greedy wrap on whitespace. A word longer than the line gets its own line
/// rather than being cut, since a URL split across lines is worse than a
/// ragged edge.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let len = line.chars().count();
            if !line.is_empty() && len + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut line));
            } else if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    if lines.iter().all(String::is_empty) {
        lines.truncate(1);
    }
    lines
}
