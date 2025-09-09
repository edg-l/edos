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
    pub fn parse_str(value: &str) -> Result<Path, ParseError> {
        let mut components: Vec<String> = value
            .trim_start_matches("/")
            .split('/')
            .map(|x| x.to_string())
            .collect();

        if components.is_empty() {
            components.push("/".to_string());
        }

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
                components: alloc::vec!["/".to_string()],
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
}
