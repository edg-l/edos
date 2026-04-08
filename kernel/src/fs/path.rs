use core::fmt;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use thiserror::Error;

/// Absolute resolved path
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path {
    components: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("invalid")]
    Invalid,
}

impl Path {
    pub fn parse(value: &str) -> Result<Path, ParseError> {
        let components: Vec<String> = value
            .split('/')
            .filter(|c| !c.is_empty())
            .map(|c| c.to_string())
            .collect();

        Ok(Path { components })
    }

    /// Return a normalized path with `.` and `..` resolved.
    /// - Absolute only (always starts at `/`).
    /// - `..` at root keeps root.
    pub fn normalize(&self) -> Path {
        let mut stack: Vec<String> = Vec::new();

        for comp in &self.components {
            match comp.as_str() {
                "" | "." => {
                    // skip empty or "."
                }
                ".." => {
                    if stack.is_empty() {
                        // at root, stay root
                    } else {
                        stack.pop();
                    }
                }
                other => stack.push(other.to_string()),
            }
        }

        if stack.is_empty() {
            Path {
                components: alloc::vec![],
            }
        } else {
            Path { components: stack }
        }
    }

    pub fn filename(&self) -> String {
        if self.is_root() {
            String::new()
        } else {
            self.components.last().unwrap().clone()
        }
    }

    pub fn components(&self) -> &[String] {
        &self.components
    }

    pub fn is_root(&self) -> bool {
        self.components.is_empty() || (self.components.len() == 1 && self.components[0] == "/")
    }

    pub fn join(&self, other: &str) -> Self {
        let components: Vec<String> = other
            .split('/')
            .filter(|c| !c.is_empty())
            .map(|c| c.to_string())
            .collect();

        let mut c = self.clone();

        c.components.extend(components);

        c
    }

    /// Returns true if `self` starts with the given `base` path components.
    pub fn starts_with(&self, base: &Path) -> bool {
        if base.components.len() > self.components.len() {
            return false;
        }
        for (i, comp) in base.components.iter().enumerate() {
            if self.components[i] != *comp {
                return false;
            }
        }
        true
    }

    /// Returns a new Path with the `base` prefix stripped if present, otherwise returns `self` clone.
    pub fn strip_prefix(&self, base: &Path) -> Path {
        if !self.starts_with(base) {
            return self.clone();
        }
        let comps = self.components[base.components.len()..].to_vec();
        Path { components: comps }
    }

    pub fn parent(&self) -> Option<Path> {
        if !self.components.is_empty() {
            Some(Self {
                components: self.components[0..(self.components.len() - 1)].to_vec(),
            })
        } else {
            None
        }
    }

    pub fn is_direct_parent(&self, other: &Path) -> bool {
        if !other.starts_with(self) {
            return false;
        }

        let stripped = other.strip_prefix(self);

        stripped.components.len() == 1
    }

    /// Return the number of path components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Return the last path component as a string slice, or None for root.
    pub fn last_component(&self) -> Option<&str> {
        self.components.last().map(|s| s.as_str())
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.components.join("/");
        write!(f, "/{}", s)
    }
}
