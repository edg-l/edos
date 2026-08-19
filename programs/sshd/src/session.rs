//! The connection protocol, RFC 4254: one `session` channel and a shell wired
//! to it.
//!
//! The loop waits on the socket and the shell's output together, which is the
//! whole reason this can be one thread per connection: neither side is polled,
//! and neither can starve the other.
//!
//! Channel flow control is honoured in both directions. The shell is left out
//! of the poll set entirely when the client's window is closed, so
//! backpressure from a client that has stopped reading turns into backpressure
//! on the shell rather than into an unbounded buffer here.

use std::thread;
use std::time::{Duration, Instant};

use edos_lib::io::{PollState, SelectFd, poll, set_winsize};
use edos_lib::process::{self, waitpid_nonblocking};

use crate::config::Config;
use crate::error::{Error, Result, protocol_err};
use crate::hostkey::HostKey;
use crate::kex::{self, Versions};
use crate::packet::{
    DISCONNECT_BY_APPLICATION, MSG_CHANNEL_CLOSE, MSG_CHANNEL_DATA, MSG_CHANNEL_EOF,
    MSG_CHANNEL_FAILURE, MSG_CHANNEL_OPEN, MSG_CHANNEL_OPEN_CONFIRMATION, MSG_CHANNEL_OPEN_FAILURE,
    MSG_CHANNEL_REQUEST, MSG_CHANNEL_SUCCESS, MSG_CHANNEL_WINDOW_ADJUST, MSG_GLOBAL_REQUEST,
    MSG_REQUEST_FAILURE, Transport,
};
use crate::wire::{Reader, Writer};

/// How much unacknowledged data the client may send us. Large enough that a
/// paste never stalls, small enough to be a bound.
const LOCAL_WINDOW: u32 = 2 * 1024 * 1024;
/// Largest channel data packet we will accept or send.
const MAX_CHANNEL_PACKET: u32 = 32 * 1024;
/// How long the poll waits before looking at the child again, in milliseconds.
/// It only bounds how quickly an exit is noticed, since data wakes the poll.
const POLL_INTERVAL: u64 = 200;

/// How long a hung-up shell is given to act on `SIGHUP` before `SIGKILL`.
const REAP_WAIT: Duration = Duration::from_millis(500);
/// Poll interval while waiting for it. There is no "a child exited" wait that
/// takes a timeout, so this is a poll.
const REAP_POLL: Duration = Duration::from_millis(20);

/// `SSH_OPEN_UNKNOWN_CHANNEL_TYPE`, RFC 4254 §5.1.
const OPEN_UNKNOWN_CHANNEL_TYPE: u32 = 3;
/// `SSH_OPEN_RESOURCE_SHORTAGE`.
const OPEN_RESOURCE_SHORTAGE: u32 = 4;

/// Put this connection thread in a process group of its own, so that the shell
/// and everything it starts share one group that a hangup can name.
///
/// `SIGHUP` is ignored here first: the thread is a member of the group it is
/// about to signal, and the point of the signal is to end the session, not the
/// server's own connection handler. Dispositions and process groups are both
/// per thread in this kernel, so neither reaches the accept loop.
fn session_group() {
    process::sys_sigaction(process::SIGHUP, process::SIG_IGN as u64);
    process::setpgid(0, 0);
}

/// A terminal dimension the client asked for, clamped to what the winsize
/// ioctl can carry. A bare `as u16` turns 65536 columns into 0, which the
/// kernel rejects, and the request is then silently ignored in full.
fn dimension(v: u32) -> u16 {
    v.clamp(1, u16::MAX as u32) as u16
}

/// What a `pty-req` asked for, held until a shell exists to apply it to.
struct PtyRequest {
    term: String,
    cols: u16,
    rows: u16,
}

/// The shell behind the channel.
///
/// A client that asked for a pty gets one, and both directions then run over
/// the master. Without a `pty-req` the shell is wired to pipes instead: a pty's
/// line discipline rewrites the bytes passing through it, which is right for a
/// terminal and wrong for `ssh host cat file`.
struct Child {
    pid: u64,
    /// The process group the session owns, led by the shell. Everything the
    /// shell starts inherits it, so one signal ends the session's whole tree.
    pgid: u64,
    /// The shell's output. The pty master, or the read end of its stdout pipe.
    read_fd: u64,
    /// The shell's input. The same pty master, or the write end of its stdin pipe.
    write_fd: u64,
    /// Whether the two descriptors are the same object.
    is_pty: bool,
    /// Whether the shell's input is still open. It closes when the client says
    /// it has no more to send, which is what lets `cat` on the far end finish.
    input_open: bool,
}

impl Child {
    /// End the shell's input. On a pty there is nothing to close: the master is
    /// also the output, and a terminal's input never ends while it is open.
    fn close_input(&mut self) {
        if !self.is_pty && self.input_open {
            process::close(self.write_fd);
            self.input_open = false;
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        process::close(self.read_fd);
        self.close_input();

        // Closing the descriptors is not enough to end the shell: a command
        // that neither reads its input nor writes its output never notices,
        // so a disconnected `ssh host 'sleep 3600'` would run out its hour
        // with nobody attached. SIGHUP is what a hangup means.
        //
        // Aimed at the group rather than the pid, because the shell is not
        // usually what is running: `sh -c 'sleep 3600'` waits on `sleep`, and
        // killing only the shell leaves the sleep reparented to init.
        let reaped = process::waitpid_nonblocking(self.pid).is_some();
        process::kill_group(self.pgid, process::SIGHUP);
        // An interactive shell takes a process group of its own so that Ctrl+C
        // reaches a job and not the shell, which puts it and everything it runs
        // outside the session group above. That group is led by the shell, so
        // its pid names it.
        if self.pid != self.pgid {
            process::kill_group(self.pid, process::SIGHUP);
        }
        if reaped {
            return;
        }

        let deadline = Instant::now() + REAP_WAIT;
        while Instant::now() < deadline {
            if process::waitpid_nonblocking(self.pid).is_some() {
                return;
            }
            thread::sleep(REAP_POLL);
        }
        // The escalation names the shell alone. `SIGKILL` cannot be ignored,
        // so aiming it at the group would take this thread with it, and the
        // socket would never be closed.
        process::sys_kill(self.pid, process::SIGKILL);
        process::waitpid_nonblocking(self.pid);
    }
}

struct Channel {
    remote_id: u32,
    /// What the client will still accept from us.
    remote_window: u32,
    remote_max_packet: u32,
    /// What we will still accept from the client before topping it back up.
    local_window: u32,
    pty: Option<PtyRequest>,
    env: Vec<String>,
    child: Option<Child>,
    /// Set once the shell has exited, and the reason the channel is still open
    /// after it: its output has yet to be drained.
    exit_status: Option<i32>,
}

/// What one pass of [`Session::forward_output`] achieved.
#[derive(PartialEq, Eq)]
enum Output {
    /// A packet went out; there may be more.
    Sent,
    /// The client's window is shut. More may be waiting, and only a
    /// `WINDOW_ADJUST` from the client can let it through.
    WindowShut,
    /// The shell had nothing to give.
    Drained,
}

pub struct Session<'a> {
    t: &'a mut Transport,
    cfg: &'a Config,
    host: &'a HostKey,
    versions: &'a Versions,
    user: String,
    channel: Option<Channel>,
}

impl<'a> Session<'a> {
    pub fn new(
        t: &'a mut Transport,
        cfg: &'a Config,
        host: &'a HostKey,
        versions: &'a Versions,
        user: String,
    ) -> Self {
        Self {
            t,
            cfg,
            host,
            versions,
            user,
            channel: None,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        loop {
            let mut fds = vec![SelectFd {
                fd: self.t.fd(),
                interests: PollState {
                    readable: true,
                    ..PollState::default()
                },
                result: PollState::default(),
            }];

            // A closed remote window keeps the shell out of the set: nothing
            // may be sent, so reading it would only move the backlog in here.
            let shell_output = match self.channel.as_ref() {
                Some(c) if c.remote_window > 0 => c.child.as_ref().map(|ch| ch.read_fd),
                _ => None,
            };
            if let Some(read_fd) = shell_output {
                fds.push(SelectFd {
                    fd: read_fd,
                    interests: PollState {
                        readable: true,
                        ..PollState::default()
                    },
                    result: PollState::default(),
                });
            }

            if poll(&mut fds, POLL_INTERVAL) < 0 {
                return Err(Error::Io("poll failed"));
            }

            if fds[0].result.readable || fds[0].result.hangup {
                let payload = self.t.recv()?;
                if self.dispatch(&payload)? {
                    return Ok(());
                }
            }

            if fds.len() > 1 && (fds[1].result.readable || fds[1].result.hangup) {
                self.forward_output()?;
            }

            self.reap_child()?;

            // A shell that has exited may still have written more than the
            // client's window allowed, so the channel closes only once its
            // output is spent. Draining inline here instead would silently
            // truncate: nothing in that loop reads the socket, so the
            // `WINDOW_ADJUST` that would let the rest through never arrives,
            // and the client sees a short file with a clean exit status.
            if let Some(status) = self.channel.as_ref().and_then(|c| c.exit_status) {
                loop {
                    match self.forward_output()? {
                        Output::Sent => continue,
                        Output::WindowShut => break,
                        Output::Drained => {
                            self.close_channel(status)?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Handle one packet. Returns true when the connection is finished.
    fn dispatch(&mut self, payload: &[u8]) -> Result<bool> {
        if kex::is_kexinit(payload) {
            // A rekey is a full exchange in the middle of the session; the
            // client's KEXINIT is already read, so it is handed over.
            kex::run(self.t, self.host, self.versions, Some(payload.to_vec()))?;
            return Ok(false);
        }

        let mut r = Reader::new(payload);
        let msg = r.byte()?;
        match msg {
            MSG_CHANNEL_OPEN => self.channel_open(&mut r)?,
            MSG_CHANNEL_REQUEST => self.channel_request(&mut r)?,
            MSG_CHANNEL_DATA => self.channel_data(&mut r)?,
            MSG_CHANNEL_WINDOW_ADJUST => {
                let _ = r.u32()?;
                let extra = r.u32()?;
                if let Some(c) = self.channel.as_mut() {
                    c.remote_window = c.remote_window.saturating_add(extra);
                }
            }
            MSG_CHANNEL_EOF => {
                // The shell keeps running: it is its exit, not the end of its
                // input, that ends the session. Its stdin does end here, so a
                // command reading to end of input can finish.
                if let Some(child) = self.channel.as_mut().and_then(|c| c.child.as_mut()) {
                    child.close_input();
                }
            }
            MSG_CHANNEL_CLOSE => {
                if let Some(c) = self.channel.take() {
                    let mut w = Writer::msg(MSG_CHANNEL_CLOSE);
                    w.u32(c.remote_id);
                    self.t.send(&w.finish())?;
                }
                return Ok(true);
            }
            MSG_GLOBAL_REQUEST => {
                let _name = r.text()?;
                if r.bool()? {
                    self.t.send(&[MSG_REQUEST_FAILURE])?;
                }
            }
            other => {
                // RFC 4253 §11.4 wants UNIMPLEMENTED rather than a disconnect,
                // so a client that tried an extension can carry on without it.
                let mut w = Writer::msg(crate::packet::MSG_UNIMPLEMENTED);
                w.u32(0);
                self.t.send(&w.finish())?;
                if self.cfg.verbose {
                    eprintln!("sshd: ignoring message {other}");
                }
            }
        }
        Ok(false)
    }

    fn channel_open(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let kind = r.text()?.to_string();
        let remote_id = r.u32()?;
        let remote_window = r.u32()?;
        let remote_max_packet = r.u32()?;

        let reject = |t: &mut Transport, code: u32, why: &str| -> Result<()> {
            let mut w = Writer::msg(MSG_CHANNEL_OPEN_FAILURE);
            w.u32(remote_id);
            w.u32(code);
            w.text(why);
            w.text("");
            t.send(&w.finish())
        };

        if kind != "session" {
            return reject(self.t, OPEN_UNKNOWN_CHANNEL_TYPE, "only session channels");
        }
        if self.channel.is_some() {
            return reject(self.t, OPEN_RESOURCE_SHORTAGE, "one session per connection");
        }

        self.channel = Some(Channel {
            remote_id,
            remote_window,
            remote_max_packet: remote_max_packet.clamp(1, MAX_CHANNEL_PACKET),
            local_window: LOCAL_WINDOW,
            pty: None,
            env: Vec::new(),
            child: None,
            exit_status: None,
        });

        let mut w = Writer::msg(MSG_CHANNEL_OPEN_CONFIRMATION);
        w.u32(remote_id);
        w.u32(0); // our channel id
        w.u32(LOCAL_WINDOW);
        w.u32(MAX_CHANNEL_PACKET);
        self.t.send(&w.finish())
    }

    fn channel_request(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let _local_id = r.u32()?;
        let kind = r.text()?.to_string();
        let want_reply = r.bool()?;

        let ok = match kind.as_str() {
            "pty-req" => {
                let term = r.text()?.to_string();
                let cols = dimension(r.u32()?);
                let rows = dimension(r.u32()?);
                let Some(c) = self.channel.as_mut() else {
                    return protocol_err!("pty-req for a channel that is not open");
                };
                c.pty = Some(PtyRequest { term, cols, rows });
                true
            }
            "env" => {
                let name = r.text()?.to_string();
                let value = r.text()?.to_string();
                if let Some(c) = self.channel.as_mut() {
                    c.env.push(format!("{name}={value}"));
                }
                true
            }
            "shell" => self.start(None)?,
            "exec" => {
                let command = r.text()?.to_string();
                self.start(Some(command))?
            }
            "window-change" => {
                let cols = dimension(r.u32()?);
                let rows = dimension(r.u32()?);
                if let Some(c) = self.channel.as_mut() {
                    if let Some(pty) = c.pty.as_mut() {
                        pty.cols = cols;
                        pty.rows = rows;
                    }
                    if let Some(child) = c.child.as_ref()
                        && child.is_pty
                    {
                        set_winsize(child.write_fd, cols, rows);
                    }
                }
                // RFC 4254 §6.7: window-change never gets a reply.
                false
            }
            _ => false,
        };

        if want_reply {
            let remote_id = self.channel.as_ref().map_or(0, |c| c.remote_id);
            let mut w = Writer::msg(if ok {
                MSG_CHANNEL_SUCCESS
            } else {
                MSG_CHANNEL_FAILURE
            });
            w.u32(remote_id);
            self.t.send(&w.finish())?;
        }
        Ok(())
    }

    /// Start the shell, either interactively or for a single command.
    fn start(&mut self, command: Option<String>) -> Result<bool> {
        let Some(channel) = self.channel.as_mut() else {
            return protocol_err!("shell requested for a channel that is not open");
        };
        if channel.child.is_some() {
            return Ok(false);
        }

        // stdin, stdout and stderr for the child, and the two descriptors this
        // side keeps.
        let (child_in, child_out, read_fd, write_fd, is_pty) = match channel.pty.as_ref() {
            Some(pty) => {
                let Some((master_fd, slave_fd)) = process::openpty() else {
                    return Ok(false);
                };
                set_winsize(slave_fd, pty.cols, pty.rows);
                (slave_fd, slave_fd, master_fd, master_fd, true)
            }
            None => {
                let Some((stdin_read, stdin_write)) = process::pipe() else {
                    return Ok(false);
                };
                let Some((stdout_read, stdout_write)) = process::pipe() else {
                    process::close(stdin_read);
                    process::close(stdin_write);
                    return Ok(false);
                };
                (stdin_read, stdout_write, stdout_read, stdin_write, false)
            }
        };

        let term = channel
            .pty
            .as_ref()
            .map_or("dumb", |p| p.term.as_str())
            .to_string();
        let mut env = vec![
            format!("TERM={term}"),
            format!("USER={}", self.user),
            format!("LOGNAME={}", self.user),
            format!("HOME={}", self.cfg.home),
            format!("SHELL={}", self.cfg.shell),
            "PATH=/bin".to_string(),
        ];
        env.extend(channel.env.iter().cloned());
        let env_refs: Vec<&str> = env.iter().map(String::as_str).collect();

        let args: Vec<&str> = match command.as_deref() {
            Some(cmd) => vec!["-c", cmd],
            None => Vec::new(),
        };
        // The session's process group has to exist before the shell does.
        // Setting it afterwards loses a race that `ssh host 'sleep 3600'`
        // wins every time: `sh -c` spawns its command immediately, so the
        // grandchild inherits the old group and outlives the hangup.
        // Everything spawned from this thread inherits the group instead.
        session_group();

        let pid = process::spawn_with_envp(
            &self.cfg.shell,
            &args,
            &env_refs,
            child_in,
            child_out,
            child_out,
        );
        // The child's ends belong to the child now; holding them here would
        // keep a pipe from ever reporting end of file.
        process::close(child_in);
        if child_out != child_in {
            process::close(child_out);
        }

        if pid == u64::MAX {
            process::close(read_fd);
            if !is_pty {
                process::close(write_fd);
            }
            eprintln!("sshd: cannot start {}", self.cfg.shell);
            return Ok(false);
        }
        let pgid = process::getpgid(0);
        channel.child = Some(Child {
            pid,
            pgid: if pgid > 0 { pgid as u64 } else { pid },
            read_fd,
            write_fd,
            is_pty,
            input_open: true,
        });
        Ok(true)
    }

    /// Client input goes to the shell, and its window is topped back up.
    fn channel_data(&mut self, r: &mut Reader<'_>) -> Result<()> {
        let _local_id = r.u32()?;
        let data = r.string()?;
        let Some(channel) = self.channel.as_mut() else {
            return Ok(());
        };

        if data.len() as u32 > channel.local_window {
            return protocol_err!("client exceeded its window by {} bytes", data.len());
        }
        channel.local_window -= data.len() as u32;

        // A shell that exited leaves a pipe with no reader, and writing to one
        // fails rather than blocking. Ending the input here keeps every later
        // packet from retrying it. (`sshd` ignores `SIGPIPE` per connection
        // thread, so the failure arrives as a return value rather than as the
        // death of this thread.)
        let mut input_gone = false;
        if let Some(child) = channel.child.as_ref().filter(|c| c.input_open) {
            let mut written = 0;
            while written < data.len() {
                let n = process::write(child.write_fd, &data[written..]);
                if n < 0 {
                    input_gone = true;
                    break;
                }
                if n == 0 {
                    break;
                }
                written += n as usize;
            }
        }
        if input_gone && let Some(child) = channel.child.as_mut() {
            child.close_input();
        }

        if channel.local_window < LOCAL_WINDOW / 2 {
            let extra = LOCAL_WINDOW - channel.local_window;
            channel.local_window = LOCAL_WINDOW;
            let remote_id = channel.remote_id;
            let mut w = Writer::msg(MSG_CHANNEL_WINDOW_ADJUST);
            w.u32(remote_id);
            w.u32(extra);
            self.t.send(&w.finish())?;
        }
        Ok(())
    }

    /// Move whatever the shell has written to the client, up to what its window
    /// and packet size allow.
    ///
    /// The three outcomes have to be told apart: a caller draining the last of
    /// a dead shell's output must keep going while the window is merely shut,
    /// and stop only when the shell has nothing left.
    fn forward_output(&mut self) -> Result<Output> {
        let Some(channel) = self.channel.as_mut() else {
            return Ok(Output::Drained);
        };
        let Some(child) = channel.child.as_ref() else {
            return Ok(Output::Drained);
        };

        let room = channel.remote_window.min(channel.remote_max_packet) as usize;
        if room == 0 {
            return Ok(Output::WindowShut);
        }
        let mut buf = vec![0u8; room];
        let n = process::read(child.read_fd, &mut buf);
        if n <= 0 {
            return Ok(Output::Drained);
        }
        let n = n as usize;
        channel.remote_window -= n as u32;
        let remote_id = channel.remote_id;

        let mut w = Writer::msg(MSG_CHANNEL_DATA);
        w.u32(remote_id);
        w.string(&buf[..n]);
        self.t.send(&w.finish())?;
        Ok(Output::Sent)
    }

    /// Record an exited shell's status. The channel stays open: what the shell
    /// wrote before exiting is still to be forwarded, and the window it needs
    /// may still be shut.
    fn reap_child(&mut self) -> Result<()> {
        let Some(channel) = self.channel.as_mut() else {
            return Ok(());
        };
        if channel.exit_status.is_some() {
            return Ok(());
        }
        let Some(child) = channel.child.as_ref() else {
            return Ok(());
        };
        let Some(status) = waitpid_nonblocking(child.pid) else {
            return Ok(());
        };
        channel.exit_status = Some(status);
        Ok(())
    }

    /// Close a channel whose shell has exited and whose output is spent.
    fn close_channel(&mut self, status: i32) -> Result<()> {
        let Some(channel) = self.channel.as_mut() else {
            return Ok(());
        };
        let remote_id = channel.remote_id;

        let mut w = Writer::msg(MSG_CHANNEL_REQUEST);
        w.u32(remote_id);
        w.text("exit-status");
        w.bool(false);
        w.u32(status as u32);
        self.t.send(&w.finish())?;

        let mut w = Writer::msg(MSG_CHANNEL_EOF);
        w.u32(remote_id);
        self.t.send(&w.finish())?;

        let mut w = Writer::msg(MSG_CHANNEL_CLOSE);
        w.u32(remote_id);
        self.t.send(&w.finish())?;
        self.channel = None;
        Ok(())
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if self.channel.is_some() {
            self.t
                .disconnect(DISCONNECT_BY_APPLICATION, "session ended");
        }
    }
}
