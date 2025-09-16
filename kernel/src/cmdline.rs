use core::ffi::CStr;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

/// Parsed command line parameters from bootloader
#[derive(Debug, Clone)]
pub struct ParsedCmdline {
    /// Root filesystem device specification (e.g., "UUID=87654321-4321-8765-cba9-987654321fed")
    pub root: Option<String>,
    /// Root filesystem type (e.g., "fat32", "ext4")
    pub rootfstype: Option<String>,
    /// Additional parameters as key-value pairs
    pub other_params: Vec<(String, Option<String>)>,
}

impl ParsedCmdline {
    /// Parse command line from a C string
    pub fn parse(cmdline: &CStr) -> Self {
        let cmdline_str = cmdline.to_str().unwrap_or("");
        Self::parse_str(cmdline_str)
    }

    /// Parse command line from a string slice
    pub fn parse_str(cmdline: &str) -> Self {
        let mut parsed = Self {
            root: None,
            rootfstype: None,
            other_params: Vec::new(),
        };

        // Split by whitespace and process each parameter
        for param in cmdline.split_whitespace() {
            if let Some((key, value)) = param.split_once('=') {
                match key {
                    "root" => parsed.root = Some(value.to_string()),
                    "rootfstype" => parsed.rootfstype = Some(value.to_string()),
                    _ => parsed
                        .other_params
                        .push((key.to_string(), Some(value.to_string()))),
                }
            } else {
                // Parameter without value (flag)
                parsed.other_params.push((param.to_string(), None));
            }
        }

        parsed
    }
}
