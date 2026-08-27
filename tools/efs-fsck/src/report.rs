#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Journal,
    Superblock,
    Bgd,
    InodeBitmap,
    BlockBitmap,
    Inode,
    DirTree,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Journal => "journal",
            Category::Superblock => "superblock",
            Category::Bgd => "bgd",
            Category::InodeBitmap => "inode-bitmap",
            Category::BlockBitmap => "block-bitmap",
            Category::Inode => "inode",
            Category::DirTree => "dir-tree",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub category: Category,
    pub message: String,
    pub fixable: bool,
    pub context: Option<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, finding: Finding) {
        self.findings.push(finding);
    }

    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    pub fn count_fixable(&self) -> usize {
        self.findings.iter().filter(|f| f.fixable).count()
    }

    pub fn by_category(&self, cat: Category) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(move |f| f.category == cat)
    }

    pub fn print(&self, verbose: bool) {
        if self.findings.is_empty() {
            println!("No issues found.");
            return;
        }
        for f in &self.findings {
            let level = match f.severity {
                Severity::Info => "INFO",
                Severity::Warning => "WARN",
                Severity::Error => "ERROR",
            };
            if f.severity == Severity::Info && !verbose {
                continue;
            }
            print!("[{}][{}] {}", level, f.category.as_str(), f.message);
            if f.fixable {
                print!(" (fixable)");
            }
            if let Some(ref ctx) = f.context {
                print!(" -- {}", ctx);
            }
            println!();
        }
    }
}
