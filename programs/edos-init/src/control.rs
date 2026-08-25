//! Runtime control: the state of every service, the FIFO commands arrive on,
//! and the file `svc` reads them back from.
//!
//! The shape is daemontools' and runit's, for the reason they chose it: the
//! program that owns `waitpid` and the restart backoff is the only one that can
//! act on a service, so control is a message to it rather than work done by the
//! caller. A FIFO is the channel because it is the one thing a program with no
//! relationship to init can open by name, and because init can hold it open
//! `O_RDWR` and so never see end of file between writers.
//!
//! Replies go the other way as state on disk, not down a second channel: init
//! rewrites [`STATUS_FILE`] whenever anything changes, and `svc list` and
//! `svc status` are ordinary reads of it. A FIFO carries one direction, and
//! giving each caller a private reply channel would be a lot of machinery for
//! something a file already says.

use std::fmt;
use std::fs;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};

use edos_lib::io;

/// Where init listens for commands.
pub const CONTROL_FIFO: &str = "/var/run/svc.ctl";

/// Where init publishes what every service is doing.
pub const STATUS_FILE: &str = "/var/run/svc.status";

/// The directory both live in, created at startup because `/var` is writable
/// but need not have been used before.
pub const RUN_DIR: &str = "/var/run";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Waiting for the devices it requires, before its first spawn.
    Waiting,
    Running,
    /// Down because it was told to be, and not to be restarted.
    Stopped,
    /// Down between restarts, serving out its backoff.
    Backoff,
    /// Given up on after too many rapid failures, or never started because it
    /// is not configured.
    Failed,
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RunState::Waiting => "waiting",
            RunState::Running => "running",
            RunState::Stopped => "stopped",
            RunState::Backoff => "backoff",
            RunState::Failed => "failed",
        })
    }
}

pub struct Entry {
    /// What the operator wants. The supervisor thread moves `state` toward it.
    pub want_up: bool,
    pub state: RunState,
    /// The pid while running, 0 otherwise.
    pub pid: u64,
    /// Consecutive failures that were fast enough to count as failures to
    /// start; reset by a healthy run and by an explicit start.
    pub failures: u32,
    /// Bumped by every start or restart request, so a supervisor serving out a
    /// backoff can tell "still the same wait" from "start now".
    pub generation: u64,
}

pub struct Registry {
    /// Kept in the order services were loaded, which is the order the status
    /// file lists them; a service count small enough that a scan is cheaper
    /// than a map, and stable output is worth more than the lookup.
    entries: Vec<(String, Entry)>,
}

impl Registry {
    fn get_mut(&mut self, name: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, e)| e)
    }

    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, e)| e)
    }
}

/// The state every supervisor thread and the control thread share.
pub struct Control {
    registry: Mutex<Registry>,
    /// Woken whenever anything in the registry changes, so a supervisor waiting
    /// to be started, or serving out a backoff, does not have to poll for it.
    changed: Condvar,
}

impl Control {
    pub fn new(names: &[String]) -> Arc<Self> {
        let entries = names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    Entry {
                        want_up: true,
                        state: RunState::Waiting,
                        pid: 0,
                        failures: 0,
                        generation: 0,
                    },
                )
            })
            .collect();
        Arc::new(Self {
            registry: Mutex::new(Registry { entries }),
            changed: Condvar::new(),
        })
    }

    /// Apply `f` to one service's entry, then publish and wake anyone waiting.
    ///
    /// Every mutation goes through here so that no path can change the state
    /// and leave the status file describing the old one.
    pub fn update<R>(&self, name: &str, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let result = registry.get_mut(name).map(f);
        if result.is_some() {
            publish(&registry);
        }
        drop(registry);
        self.changed.notify_all();
        result
    }

    /// Read one service's entry.
    pub fn with<R>(&self, name: &str, f: impl FnOnce(&Entry) -> R) -> Option<R> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        registry.entry(name).map(f)
    }

    /// Block until `ready` holds for this service's entry.
    ///
    /// The wait is what makes a stopped service cost nothing: its supervisor
    /// parks here rather than waking to ask whether anything has changed.
    pub fn wait_until(&self, name: &str, ready: impl Fn(&Entry) -> bool) {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match registry.entry(name) {
                Some(entry) if ready(entry) => return,
                // A name with no entry cannot become ready; returning beats
                // waiting for something nothing will ever signal.
                None => return,
                _ => {}
            }
            registry = self
                .changed
                .wait(registry)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Publish the current state without changing anything, for the first write
    /// at startup.
    pub fn publish_now(&self) {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        publish(&registry);
    }
}

/// Rewrite the status file.
///
/// Written whole and replaced by rename, so a reader never sees half a table:
/// `svc` polls this file to find out whether the command it sent took effect,
/// and a truncated read there would look like a service that had vanished.
fn publish(registry: &Registry) {
    let mut text = String::new();
    for (name, entry) in &registry.entries {
        text.push_str(&format!(
            "{} {} {} {}\n",
            name, entry.state, entry.pid, entry.failures
        ));
    }

    let temp = format!("{STATUS_FILE}.new");
    let written = fs::File::create(&temp).and_then(|mut f| f.write_all(text.as_bytes()));
    if let Err(e) = written {
        eprintln!("init: {temp}: {e}");
        return;
    }
    if let Err(e) = fs::rename(&temp, STATUS_FILE) {
        eprintln!("init: {STATUS_FILE}: {e}");
    }
}

/// What a line on the control FIFO asks for.
enum Command {
    Start,
    Stop,
    Restart,
}

fn parse(line: &str) -> Option<(Command, &str)> {
    let mut parts = line.split_whitespace();
    let command = match parts.next()? {
        "start" => Command::Start,
        "stop" => Command::Stop,
        "restart" => Command::Restart,
        other => {
            eprintln!("init: control: unknown command {other:?}");
            return None;
        }
    };
    let name = parts.next()?;
    Some((command, name))
}

/// Make the directory the status file and the FIFO live in.
///
/// Called before any service starts, because publishing is what a supervisor
/// does on its way up: a directory created later by the control thread would
/// lose every state change that happened before it won that race, and leave
/// services that came up first reading as though they never had.
pub fn prepare() {
    if let Err(e) = fs::create_dir_all(RUN_DIR) {
        eprintln!("init: {RUN_DIR}: {e}; no runtime control");
    }
}

/// Serve the control FIFO forever.
///
/// Opened `O_RDWR` so init holds a write end of its own: with only a read end,
/// every `svc` that closed its side would put the pipe at end of file and this
/// loop would spin on a hangup nothing would clear.
pub fn serve(control: Arc<Control>) {
    // A FIFO left behind by a previous boot is a name, not a channel: the
    // buffer went with the kernel that held it. Remaking it costs nothing and
    // means a stale one of the wrong type cannot wedge control.
    let _ = fs::remove_file(CONTROL_FIFO);
    if io::mkfifo(CONTROL_FIFO).is_err() {
        eprintln!(
            "init: {CONTROL_FIFO}: {:?}; no runtime control",
            io::last_errno()
        );
        return;
    }

    let Ok(fd) = io::open(CONTROL_FIFO, io::O_RDWR).inspect_err(|e| {
        eprintln!("init: {CONTROL_FIFO}: {e:?}; no runtime control");
    }) else {
        return;
    };

    let mut pending = String::new();
    let mut buf = [0u8; 512];
    loop {
        let Some(n) = io::sys_read(fd, &mut buf).ok().filter(|&n| n > 0) else {
            // O_RDWR means end of file cannot happen, so anything here is a
            // real error; backing off beats spinning on it.
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        };
        pending.push_str(&String::from_utf8_lossy(&buf[..n]));

        while let Some(end) = pending.find('\n') {
            let line: String = pending.drain(..=end).collect();
            let Some((command, name)) = parse(line.trim()) else {
                continue;
            };
            apply(&control, command, name);
        }

        // A writer that sends no newline must not be able to grow this buffer
        // without limit.
        if pending.len() > 4096 {
            eprintln!("init: control: discarding an overlong line");
            pending.clear();
        }
    }
}

fn apply(control: &Control, command: Command, name: &str) {
    let acted = control.update(name, |entry| match command {
        Command::Start => {
            entry.want_up = true;
            entry.failures = 0;
            entry.generation += 1;
            None
        }
        Command::Stop => {
            entry.want_up = false;
            // Taken out and signalled after the lock goes: the supervisor's
            // `waitpid` returns and it sees `want_up` already false, so it
            // parks instead of restarting.
            (entry.state == RunState::Running).then_some(entry.pid)
        }
        Command::Restart => {
            entry.want_up = true;
            entry.failures = 0;
            entry.generation += 1;
            (entry.state == RunState::Running).then_some(entry.pid)
        }
    });

    match acted {
        None => eprintln!("init: control: no service named {name:?}"),
        Some(Some(pid)) => {
            let _ = edos_lib::process::kill(pid, edos_lib::process::SIGTERM);
        }
        Some(None) => {}
    }
}
