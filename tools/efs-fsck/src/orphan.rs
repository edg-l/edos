//! The orphan chain: inodes that lost their last name and whose storage was not
//! freed before the filesystem was unmounted.
//!
//! An inode is on the chain from the moment its directory entry goes away until
//! its blocks and inode are freed (`doc/efs.md` §14). A mount walks the chain and
//! finishes those deletions, so a chain found here means either an unclean
//! shutdown that no mount has followed, or an image inspected between the two.
//!
//! The distinction matters for what the checker says. An inode on the chain has
//! no name *by design* and is not damage: it is a deletion the filesystem already
//! committed to, and completing it is not destructive. An allocated inode with no
//! name that is **not** on the chain is a genuine leak, and freeing it needs the
//! prompt.

use std::collections::BTreeSet;
use std::io;

use efs_common::{COMPAT_ORPHAN_LIST, EfsBlockGroupDesc, EfsInode, EfsSuperblock};

use crate::disk::Disk;
use crate::layout::inode_location;
use crate::report::{Category, Finding, Report, Severity};

/// The inodes the chain names, for the per-inode checks in the later phases.
pub struct OrphanChain {
    pub set: BTreeSet<u64>,
}

impl OrphanChain {
    fn empty() -> Self {
        OrphanChain {
            set: BTreeSet::new(),
        }
    }
}

/// Read one inode straight from the inode table.
fn read_inode(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
    ino: u64,
) -> io::Result<Option<EfsInode>> {
    let block_size = disk.block_size as usize;
    let Some((block, offset_in_block)) = inode_location(sb, bgds, ino, block_size) else {
        return Ok(None);
    };
    disk.read_struct_at(block, offset_in_block).map(Some)
}

/// Walk the chain from the superblock's head, reporting anything structurally
/// wrong with it.
///
/// The walk is bounded by the inode count and by a visited set, so a cycle or a
/// wild pointer ends it with a finding instead of running forever.
pub fn walk(
    disk: &mut Disk,
    sb: &EfsSuperblock,
    bgds: &[EfsBlockGroupDesc],
) -> io::Result<(OrphanChain, Report)> {
    let mut report = Report::new();
    let head = sb.last_orphan as u64;
    let total_inodes = sb.total_inodes;
    let has_feature = sb.compatible_features & COMPAT_ORPHAN_LIST != 0;

    if head == 0 {
        return Ok((OrphanChain::empty(), report));
    }

    if !has_feature {
        report.push(Finding {
            severity: Severity::Warning,
            category: Category::Superblock,
            message: format!(
                "last_orphan is {head} but COMPAT_ORPHAN_LIST is not set; \
                 treating the chain as present"
            ),
            fixable: false,
            context: None,
        });
    }

    let mut set: BTreeSet<u64> = BTreeSet::new();
    let mut ino = head;
    let mut complete = false;

    while ino != 0 {
        if ino > total_inodes {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!(
                    "orphan chain reaches inode {ino}, beyond the {total_inodes} the \
                     filesystem has"
                ),
                fixable: false,
                context: None,
            });
            break;
        }
        if !set.insert(ino) {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!("orphan chain loops back to inode {ino}"),
                fixable: false,
                context: None,
            });
            break;
        }

        let Some(inode) = read_inode(disk, sb, bgds, ino)? else {
            report.push(Finding {
                severity: Severity::Error,
                category: Category::DirTree,
                message: format!("orphan chain reaches inode {ino}, which has no inode table"),
                fixable: false,
                context: None,
            });
            break;
        };
        let link_count = inode.link_count;
        if link_count != 0 {
            report.push(Finding {
                severity: Severity::Warning,
                category: Category::DirTree,
                message: format!("inode {ino} is on the orphan chain with link_count {link_count}"),
                fixable: false,
                context: Some(
                    "a chained inode has lost its last name, so a non-zero count is \
                     stale rather than a second name"
                        .to_string(),
                ),
            });
        }

        let next = inode.orphan_next as u64;
        if next == 0 {
            complete = true;
        }
        ino = next;
    }

    if !set.is_empty() {
        report.push(Finding {
            severity: Severity::Info,
            category: Category::DirTree,
            message: format!(
                "{} inode(s) pending deletion on the orphan chain",
                set.len()
            ),
            fixable: false,
            context: Some(
                "an unclean shutdown interrupted these deletions; a mount finishes them, \
                 and so does --repair"
                    .to_string(),
            ),
        });
    }

    if !complete {
        report.push(Finding {
            severity: Severity::Warning,
            category: Category::DirTree,
            message: "the orphan chain does not end; the inodes past the break are \
                      unreachable through it"
                .to_string(),
            fixable: false,
            context: Some(
                "they are reported as ordinary orphans instead, which --repair prompts \
                 before freeing"
                    .to_string(),
            ),
        });
    }

    Ok((OrphanChain { set }, report))
}
