use edos_lib::process::{self, ChildState};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobStatus {
    Running,
    /// Suspended by a stop signal, waiting for `fg` or `bg` to resume it.
    Stopped,
    Done(i32),
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u32,
    /// The job's process group, which is also its first process's pid.
    pub pgid: u64,
    /// Every process of the job, in pipeline order.
    pub pids: Vec<u64>,
    pub command: String,
    pub status: JobStatus,
}

/// How a foreground job ended: it finished, or it stopped and became a job.
pub enum Foreground {
    Exited(i32),
    Stopped,
}

/// Wait for every process of a foreground job, reporting the last stage's exit
/// code.
///
/// A stop of any stage stops the whole job: the signal went to the process
/// group, so the other stages are suspended too and waiting for them would
/// hang the shell.
pub fn wait_foreground(pids: &[u64]) -> Foreground {
    let mut last = 0;
    for (i, &pid) in pids.iter().enumerate() {
        match process::waitpid_untraced_blocking(pid) {
            Some(ChildState::Stopped) => return Foreground::Stopped,
            Some(ChildState::Exited(code)) if i + 1 == pids.len() => last = code,
            Some(ChildState::Exited(_)) => {}
            None => {}
        }
    }
    Foreground::Exited(last)
}

pub struct JobList {
    jobs: Vec<Job>,
    next_id: u32,
}

impl JobList {
    pub const fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    /// Record a job in the given state. Returns the job ID.
    pub fn add(&mut self, pids: Vec<u64>, command: String, status: JobStatus) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.push(Job {
            id,
            pgid: pids.first().copied().unwrap_or(0),
            pids,
            command,
            status,
        });
        id
    }

    /// The job `fg` and `bg` act on with no argument: the most recent one.
    pub fn current_id(&self) -> Option<u32> {
        self.jobs.last().map(|j| j.id)
    }

    /// Take a job out of the list so the caller can run it in the foreground.
    pub fn take(&mut self, id: u32) -> Option<Job> {
        let pos = self.jobs.iter().position(|j| j.id == id)?;
        Some(self.jobs.remove(pos))
    }

    /// Return a job taken with [`take`] to the list, keeping its ID.
    pub fn put_back(&mut self, mut job: Job, status: JobStatus) {
        job.status = status;
        self.jobs.push(job);
    }

    /// Mark a job as running again, after `bg` resumed it.
    pub fn resume(&mut self, id: u32) -> Option<Job> {
        let job = self.jobs.iter_mut().find(|j| j.id == id)?;
        job.status = JobStatus::Running;
        Some(job.clone())
    }

    /// Non-blocking reap: check all running jobs, return newly completed ones, remove them.
    ///
    /// A job that stopped while in the background is kept, marked stopped, and
    /// reported so the shell can say so.
    pub fn reap(&mut self) -> Vec<(u32, String, JobStatus)> {
        let mut changed = Vec::new();
        for job in &mut self.jobs {
            if job.status != JobStatus::Running {
                continue;
            }
            // The last stage finishing is what ends the job; earlier stages are
            // reaped with it so nothing is left behind.
            let Some(&last) = job.pids.last() else {
                continue;
            };
            match process::waitpid_untraced(last) {
                Some(ChildState::Exited(code)) => {
                    for &pid in &job.pids[..job.pids.len() - 1] {
                        process::waitpid_nonblocking(pid);
                    }
                    job.status = JobStatus::Done(code);
                    changed.push((job.id, job.command.clone(), job.status));
                }
                Some(ChildState::Stopped) => {
                    job.status = JobStatus::Stopped;
                    changed.push((job.id, job.command.clone(), job.status));
                }
                None => {}
            }
        }
        self.jobs
            .retain(|j| !matches!(j.status, JobStatus::Done(_)));
        changed
    }

    /// Block until all running jobs complete. Returns last exit code.
    pub fn wait_all(&mut self) -> i32 {
        let mut last_code = 0;
        for job in &mut self.jobs {
            if job.status == JobStatus::Running {
                for &pid in &job.pids {
                    last_code = process::waitpid(pid);
                }
                job.status = JobStatus::Done(last_code);
            }
        }
        self.jobs
            .retain(|j| !matches!(j.status, JobStatus::Done(_)));
        last_code
    }

    /// Block until a specific PID completes. Returns exit code or -1 if not found.
    pub fn wait_pid(&mut self, pid: u64) -> i32 {
        let Some(pos) = self
            .jobs
            .iter()
            .position(|j| j.pids.contains(&pid) || j.pgid == pid)
        else {
            eprintln!("wait: no such job: {}", pid);
            return -1;
        };
        match self.jobs[pos].status {
            JobStatus::Running => {
                let code = process::waitpid(pid);
                self.jobs.remove(pos);
                code
            }
            JobStatus::Stopped => -1,
            JobStatus::Done(code) => {
                self.jobs.remove(pos);
                code
            }
        }
    }

    /// Print all jobs, most recent marked `+` the way a shell does.
    pub fn print(&self) {
        let current = self.current_id();
        for job in &self.jobs {
            let mark = if Some(job.id) == current { '+' } else { '-' };
            println!(
                "[{}]{} {} {} {}",
                job.id,
                mark,
                job.pgid,
                status_word(job.status),
                job.command
            );
        }
    }
}

/// The word `jobs` and the stop notice print for a status.
pub fn status_word(status: JobStatus) -> String {
    match status {
        JobStatus::Running => "Running".to_string(),
        JobStatus::Stopped => "Stopped".to_string(),
        JobStatus::Done(code) => format!("Done({})", code),
    }
}
