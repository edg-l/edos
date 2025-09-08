use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use thiserror::Error;

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
        let mut components: Vec<String> = value.split('/').map(|x| x.to_string()).collect();

        if components.is_empty() {
            components.push("/".to_string());
        }

        Ok(Path { components })
    }
}
