//! Server configuration: a file of `keyword value` lines, overridable on the
//! command line.

use std::fs;

pub struct User {
    pub name: String,
    pub password: String,
}

pub struct Config {
    pub port: u16,
    pub shell: String,
    pub home: String,
    pub hostkey: String,
    pub users: Vec<User>,
    pub verbose: bool,
}

pub const DEFAULT_CONFIG: &str = "/etc/sshd.conf";

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 22,
            shell: "/bin/sh".to_string(),
            home: "/".to_string(),
            hostkey: "/var/sshd_host_ed25519".to_string(),
            users: Vec::new(),
            verbose: false,
        }
    }
}

impl Config {
    /// Merge `path` into this configuration. A missing file is not an error:
    /// the command line alone is a complete configuration.
    pub fn merge_file(&mut self, path: &str) -> Result<(), String> {
        let Ok(text) = fs::read_to_string(path) else {
            return Ok(());
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, char::is_whitespace);
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("").trim();
            let rest = parts.next().unwrap_or("").trim();
            let where_ = format!("{path}:{}", n + 1);
            match key {
                "port" => {
                    self.port = value
                        .parse()
                        .map_err(|_| format!("{where_}: {value:?} is not a port"))?
                }
                "shell" => self.shell = value.to_string(),
                "home" => self.home = value.to_string(),
                "hostkey" => self.hostkey = value.to_string(),
                "user" => {
                    if value.is_empty() {
                        return Err(format!("{where_}: user needs a name and a password"));
                    }
                    self.add_user(value, rest);
                }
                other => return Err(format!("{where_}: unknown keyword {other:?}")),
            }
        }
        Ok(())
    }

    /// Add a user, replacing any earlier entry of the same name so that the
    /// command line wins over the file.
    pub fn add_user(&mut self, name: &str, password: &str) {
        self.users.retain(|u| u.name != name);
        self.users.push(User {
            name: name.to_string(),
            password: password.to_string(),
        });
    }
}
