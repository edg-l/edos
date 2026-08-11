//! pstree - the thread table drawn as a tree
//!
//! `/proc/processes` carries a PPID column and every other reader prints it as
//! a number, so the supervision structure `edos-init` builds — and the fact
//! that a shell's children outlive the pipeline that started them — is
//! invisible. This renders the same table as the forest it already is.

use edos_lib::procinfo::{self, Process};
use std::collections::HashMap;
use std::process::ExitCode;

/// The characters the branches are drawn with. The default set is ASCII
/// because a tree is the one thing that must survive being redirected into a
/// file, piped through `grep`, or read on a console whose font has no box
/// drawing; `-U` opts into the nicer one.
struct Glyphs {
    dash: &'static str,
    fork: &'static str,
    tee: &'static str,
    corner: &'static str,
    vert: &'static str,
}

const ASCII: Glyphs = Glyphs {
    dash: "-",
    fork: "+",
    tee: "|-",
    corner: "`-",
    vert: "|",
};

const UTF8: Glyphs = Glyphs {
    dash: "\u{2500}",
    fork: "\u{252c}",
    tee: "\u{251c}\u{2500}",
    corner: "\u{2514}\u{2500}",
    vert: "\u{2502}",
};

struct Options {
    show_pid: bool,
    show_pgid: bool,
    show_path: bool,
    show_kernel: bool,
    sort_by_pid: bool,
    compact: bool,
    glyphs: &'static Glyphs,
    root: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            show_pid: false,
            show_pgid: false,
            show_path: false,
            show_kernel: true,
            sort_by_pid: false,
            compact: true,
            glyphs: &ASCII,
            root: None,
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: pstree [-p] [-g] [-l] [-u] [-n] [-c] [-U] [PID]");
    std::process::exit(2)
}

fn parse_args() -> Options {
    let mut options = Options::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-p" => options.show_pid = true,
            "-g" => options.show_pgid = true,
            "-l" => options.show_path = true,
            "-u" => options.show_kernel = false,
            "-n" => options.sort_by_pid = true,
            "-c" => options.compact = false,
            "-U" => options.glyphs = &UTF8,
            "-h" | "--help" => usage(),
            other if other.starts_with('-') => usage(),
            other => options.root = Some(other.parse().unwrap_or_else(|_| usage())),
        }
    }
    options
}

struct Node {
    label: String,
    children: Vec<usize>,
    /// Sort key, so a compacted group keeps the position of the siblings it
    /// replaced.
    pid: u64,
}

struct Tree {
    nodes: Vec<Node>,
    roots: Vec<usize>,
}

/// The command a name refers to, without the directory it was found in.
/// `/proc/processes` names a thread by the path it was spawned with, which is
/// most of the width of a tree that repeats it on every line.
fn command(name: &str) -> &str {
    match name.rsplit_once('/') {
        Some((_, tail)) if !tail.is_empty() => tail,
        _ => name,
    }
}

/// What a row is called in the tree. Arguments are not available to print: the
/// kernel keeps a thread's name and never its argv, so `/proc/<pid>/cmdline`
/// answers with the same path this column holds, and `-l` shows that whole
/// path rather than pretending to more.
fn label(process: &Process, options: &Options) -> String {
    let mut text = if options.show_path {
        process.name.clone()
    } else {
        command(&process.name).to_string()
    };

    let mut annotations: Vec<String> = Vec::new();
    if options.show_pid {
        annotations.push(process.pid.to_string());
    }
    if options.show_pgid {
        annotations.push(format!("pgid {}", process.pgid));
    }
    if !annotations.is_empty() {
        text.push_str(&format!("({})", annotations.join(",")));
    }
    text
}

fn build(processes: Vec<Process>, options: &Options) -> Tree {
    let mut index: HashMap<u64, usize> = HashMap::with_capacity(processes.len());
    let mut nodes = Vec::with_capacity(processes.len());
    let mut parents = Vec::with_capacity(processes.len());

    for process in &processes {
        index.insert(process.pid, nodes.len());
        parents.push(process.ppid);
        nodes.push(Node {
            label: label(process, options),
            children: Vec::new(),
            pid: process.pid,
        });
    }

    // A thread whose parent is not in the table is a root: with `-u` that is
    // every process the kernel started, and a reparented orphan lands here too
    // rather than disappearing. A thread claiming itself as its parent would
    // otherwise build a cycle the renderer never leaves.
    let mut roots = Vec::new();
    for (child, &ppid) in parents.iter().enumerate() {
        match index.get(&ppid) {
            Some(&parent) if parent != child => nodes[parent].children.push(child),
            _ => roots.push(child),
        }
    }

    let mut tree = Tree { nodes, roots };
    let order = if options.sort_by_pid {
        Order::Pid
    } else {
        Order::Name
    };
    for root in tree.roots.clone() {
        sort_subtree(&mut tree, root, order);
    }
    if options.compact {
        for root in tree.roots.clone() {
            compact_subtree(&mut tree, root, options.glyphs);
        }
    }
    sort_roots(&mut tree, order);
    tree
}

#[derive(Clone, Copy, PartialEq)]
enum Order {
    Name,
    Pid,
}

fn sort_roots(tree: &mut Tree, order: Order) {
    let mut roots = std::mem::take(&mut tree.roots);
    sort_by(&mut roots, tree, order);
    tree.roots = roots;
}

fn sort_by(ids: &mut [usize], tree: &Tree, order: Order) {
    match order {
        Order::Pid => ids.sort_by_key(|&id| tree.nodes[id].pid),
        // Ties break on pid so the order is stable across refreshes, and so
        // that identical siblings end up adjacent for compaction.
        Order::Name => ids.sort_by(|&a, &b| {
            let (a, b) = (&tree.nodes[a], &tree.nodes[b]);
            a.label.cmp(&b.label).then(a.pid.cmp(&b.pid))
        }),
    }
}

fn sort_subtree(tree: &mut Tree, id: usize, order: Order) {
    let mut children = std::mem::take(&mut tree.nodes[id].children);
    for &child in &children {
        sort_subtree(tree, child, order);
    }
    sort_by(&mut children, tree, order);
    tree.nodes[id].children = children;
}

/// Replace runs of identical siblings with one `N*[...]` node, bottom up so a
/// parent sees its children's own compaction.
///
/// Only a subtree that renders on one line — a chain, where every node has at
/// most one child — is compacted, since that is the case whose collapsed form
/// is unambiguous. The kernel's per-CPU threads and a shell's repeated jobs are
/// all of this shape.
fn compact_subtree(tree: &mut Tree, id: usize, glyphs: &Glyphs) {
    let children = std::mem::take(&mut tree.nodes[id].children);
    for &child in &children {
        compact_subtree(tree, child, glyphs);
    }

    let mut compacted: Vec<usize> = Vec::with_capacity(children.len());
    let mut group_start = 0;
    while group_start < children.len() {
        let key = chain_label(tree, children[group_start], glyphs);
        let mut end = group_start + 1;
        if key.is_some() {
            while end < children.len() && chain_label(tree, children[end], glyphs) == key {
                end += 1;
            }
        }
        let count = end - group_start;
        if count > 1 {
            let pid = tree.nodes[children[group_start]].pid;
            tree.nodes.push(Node {
                label: format!("{}*[{}]", count, key.unwrap()),
                children: Vec::new(),
                pid,
            });
            compacted.push(tree.nodes.len() - 1);
        } else {
            compacted.push(children[group_start]);
        }
        group_start = end;
    }

    tree.nodes[id].children = compacted;
}

/// The one-line rendering of a subtree, or `None` if it branches.
fn chain_label(tree: &Tree, id: usize, glyphs: &Glyphs) -> Option<String> {
    let node = &tree.nodes[id];
    match node.children.as_slice() {
        [] => Some(node.label.clone()),
        [only] => Some(format!(
            "{}{}{}",
            node.label,
            glyphs.dash.repeat(3),
            chain_label(tree, *only, glyphs)?
        )),
        _ => None,
    }
}

/// Draw one subtree. `prefix` is written at the start of every continuation
/// line and its width is the column this node's label starts at, so an
/// ancestor's branch stays drawn down the left of everything under it.
fn draw(tree: &Tree, id: usize, prefix: &str, glyphs: &Glyphs, out: &mut String) {
    let node = &tree.nodes[id];
    out.push_str(&node.label);

    let children = node.children.as_slice();
    if children.is_empty() {
        out.push('\n');
        return;
    }

    // The connector sits one column past the label; children start two columns
    // past the connector, which is what makes `a---b` and `a-+-b` line up.
    let stem = format!("{}{}", prefix, " ".repeat(node.label.chars().count() + 1));
    let last_prefix = format!("{}  ", stem);

    if let [only] = children {
        out.push_str(&glyphs.dash.repeat(3));
        draw(tree, *only, &last_prefix, glyphs, out);
        return;
    }

    let inner_prefix = format!("{}{} ", stem, glyphs.vert);
    out.push_str(glyphs.dash);
    out.push_str(glyphs.fork);
    out.push_str(glyphs.dash);
    for (position, &child) in children.iter().enumerate() {
        let last = position + 1 == children.len();
        if position > 0 {
            out.push_str(&stem);
            out.push_str(if last { glyphs.corner } else { glyphs.tee });
        }
        let child_prefix = if last { &last_prefix } else { &inner_prefix };
        draw(tree, child, child_prefix, glyphs, out);
    }
}

fn main() -> ExitCode {
    let options = parse_args();

    let table = match procinfo::read_table() {
        Ok(table) => table,
        Err(e) => {
            eprintln!("pstree: /proc/processes: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let processes: Vec<Process> = table
        .processes
        .into_iter()
        .filter(|p| options.show_kernel || !p.is_kernel())
        .collect();

    let wanted = options.root;
    if let Some(pid) = wanted
        && !processes.iter().any(|p| p.pid == pid)
    {
        eprintln!("pstree: no process {}", pid);
        return ExitCode::FAILURE;
    }

    let tree = build(processes, &options);
    let roots: Vec<usize> = match wanted {
        Some(pid) => tree
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.pid == pid)
            .map(|(id, _)| id)
            .take(1)
            .collect(),
        None => tree.roots.clone(),
    };

    let mut out = String::new();
    for &root in &roots {
        draw(&tree, root, "", options.glyphs, &mut out);
    }
    print!("{}", out);
    ExitCode::SUCCESS
}
