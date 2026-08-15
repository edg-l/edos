//! What a service is, and how one is declared on disk.

use std::fs;
use std::sync::Arc;

/// Where a service declared on disk lives. One file per service, named for the
/// service: `/etc/services/httpd.conf` declares `httpd`.
pub const SERVICES_DIR: &str = "/etc/services";

/// Configuration that turns the SSH server on. It holds the only credential
/// the server has, so a system without one has no business listening.
const SSHD_CONFIG: &str = "/etc/sshd.conf";

/// When a service that has exited should be started again.
///
/// The distinction the supervisor could not previously make is between a
/// process that failed and one that finished. A terminal the user closed exits
/// 0 and means it; restarting it puts a window back on screen that the user
/// just dismissed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Restart {
    /// Always bring it back. Right for anything the session cannot be used
    /// without -- the compositor, the panel.
    #[default]
    Always,
    /// Bring it back only if it failed. A clean exit is taken at its word.
    OnFailure,
    /// Never bring it back on its own; `svc start` is the only way.
    Never,
}

pub struct Service {
    /// What the service is called, on the command line and in the status file.
    /// Taken from the file name of a declared service, or from the binary's
    /// name for a compiled-in one.
    pub name: String,
    /// Binary to run.
    pub command: String,
    /// Arguments handed to it, not counting `argv[0]`.
    pub args: Vec<String>,
    /// Whether the session is usable without it. A non-essential service that
    /// exhausts its restarts is left dead; an essential one is a louder problem
    /// but is still not worth rebooting the machine over.
    pub essential: bool,
    /// Whether this service manages other processes' windows, and so needs the
    /// privilege to move, resize, frame, minimize and focus them.
    ///
    /// Granted per spawn, since the privilege is per pid and dies with the
    /// process. Init holds it because the kernel starts init and nothing else,
    /// which makes "what a session is" init's decision rather than a race
    /// between whichever program claims it first.
    pub shell: bool,
    /// Device nodes that must exist before the service is worth spawning.
    ///
    /// Drivers register their `/dev` entries from kthreads, so a node appears
    /// some time after userspace starts rather than before it. A service that
    /// opens one during startup otherwise races the driver and dies, and a
    /// service that treats the open as optional comes up permanently without
    /// that device. Waiting here keeps both out of every service.
    pub requires: Vec<String>,
    /// What to do when it exits. See [`Restart`].
    pub restart: Restart,
    /// A file whose absence means the service is not configured, and so is not
    /// started at all.
    ///
    /// Distinct from `requires`, which waits and then starts the service
    /// anyway: this is a decision, not a race. A network service with no
    /// credentials configured would only exit and be restarted until its
    /// failure budget ran out, logging on every boot of a system whose owner
    /// never asked for it.
    pub enabled_by: Option<String>,
}

impl Service {
    fn new(name: &str, command: &str) -> Self {
        Self {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            essential: false,
            shell: false,
            restart: Restart::default(),
            requires: Vec::new(),
            enabled_by: None,
        }
    }

    /// Whether this service is configured to run at all.
    pub fn enabled(&self) -> bool {
        match &self.enabled_by {
            Some(path) => fs::metadata(path).is_ok(),
            None => true,
        }
    }

    /// Parse one `keyword value` file. The shape is `/etc/sshd.conf`'s: a
    /// keyword, a space, and the rest of the line as its value.
    fn parse(name: &str, path: &str, text: &str) -> Result<Self, String> {
        let mut service = Service::new(name, "");
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(2, char::is_whitespace);
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("").trim();
            let where_ = format!("{path}:{}", n + 1);
            match key {
                "command" => service.command = value.to_string(),
                "args" => service.args = value.split_whitespace().map(str::to_string).collect(),
                "requires" => {
                    service.requires = value.split_whitespace().map(str::to_string).collect()
                }
                "enabled_by" => service.enabled_by = Some(value.to_string()),
                "restart" => {
                    service.restart = match value {
                        "always" => Restart::Always,
                        "on-failure" => Restart::OnFailure,
                        "never" => Restart::Never,
                        other => {
                            return Err(format!(
                                "{where_}: restart wants always, on-failure or never, got {other:?}"
                            ));
                        }
                    }
                }
                "essential" => service.essential = parse_bool(value, &where_)?,
                "shell" => service.shell = parse_bool(value, &where_)?,
                other => return Err(format!("{where_}: unknown keyword {other:?}")),
            }
        }
        if service.command.is_empty() {
            return Err(format!("{path}: no command"));
        }
        Ok(service)
    }
}

fn parse_bool(value: &str, where_: &str) -> Result<bool, String> {
    match value {
        "yes" | "true" | "1" => Ok(true),
        "no" | "false" | "0" => Ok(false),
        other => Err(format!("{where_}: {other:?} is not yes or no")),
    }
}

/// The desktop session, which is what a system with no `/etc/services` is.
///
/// Compiled in rather than shipped as files so that a filesystem with nothing
/// on it still boots to a usable machine, and so that a mistake in a config
/// file cannot cost the session the window manager. Declared services add to
/// these; they do not replace them.
fn defaults() -> Vec<Service> {
    vec![
        Service {
            essential: true,
            shell: true,
            requires: vec!["/dev/mouse".to_string(), "/dev/kbd".to_string()],
            ..Service::new("edos-wm", "/bin/edos-wm")
        },
        Service {
            shell: true,
            ..Service::new("edos-taskbar", "/bin/edos-taskbar")
        },
        Service {
            // A terminal the user closed stays closed. It exits 0 to say so,
            // and the panel menu is how another one is opened.
            restart: Restart::OnFailure,
            ..Service::new("edos-terminal", "/bin/edos-terminal")
        },
        Service {
            enabled_by: Some(SSHD_CONFIG.to_string()),
            ..Service::new("sshd", "/bin/sshd")
        },
    ]
}

/// Every service this system has: the session, plus whatever `/etc/services`
/// declares.
///
/// A file that does not parse is reported and skipped rather than fatal: init
/// coming up without one service beats init not coming up.
pub fn load() -> Vec<Arc<Service>> {
    let mut services = defaults();

    let mut declared: Vec<String> = match fs::read_dir(SERVICES_DIR) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".conf"))
            .collect(),
        // No directory is the ordinary case, not a problem to report.
        Err(_) => Vec::new(),
    };
    // Sorted so a boot is reproducible: `read_dir` returns whatever order the
    // filesystem holds, and the order services start in should not depend on
    // the order they were written.
    declared.sort();

    for file in declared {
        let path = format!("{SERVICES_DIR}/{file}");
        let name = file.trim_end_matches(".conf");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("init: {path}: {e}");
                continue;
            }
        };
        match Service::parse(name, &path, &text) {
            Ok(service) => {
                // A file naming a compiled-in service replaces it, which is how
                // the session is reconfigured without editing this program.
                services.retain(|s| s.name != service.name);
                services.push(service);
            }
            Err(e) => eprintln!("init: {e}"),
        }
    }

    services.into_iter().map(Arc::new).collect()
}
