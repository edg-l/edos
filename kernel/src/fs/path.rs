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
}
