use edos_lib::process;

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Done(i32),
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u32,
    pub pid: u64,
    pub command: String,
    pub status: JobStatus,
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

    /// Add a running background job. Returns the job ID.
    pub fn add(&mut self, pid: u64, command: String) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.push(Job {
            id,
            pid,
            command,
            status: JobStatus::Running,
        });
        id
    }

    /// Non-blocking reap: check all running jobs, return newly completed ones, remove them.
    pub fn reap(&mut self) -> Vec<(u32, String, i32)> {
        let mut completed = Vec::new();
        for job in &mut self.jobs {
            if job.status == JobStatus::Running {
                if let Some(code) = process::waitpid_nonblocking(job.pid) {
                    job.status = JobStatus::Done(code);
                    completed.push((job.id, job.command.clone(), code));
                }
            }
        }
        self.jobs.retain(|j| j.status == JobStatus::Running);
        completed
    }

    /// Block until all running jobs complete. Returns last exit code.
    pub fn wait_all(&mut self) -> i32 {
        let mut last_code = 0;
        for job in &mut self.jobs {
            if job.status == JobStatus::Running {
                let code = process::waitpid(job.pid);
                job.status = JobStatus::Done(code);
                last_code = code;
            }
        }
        self.jobs.clear();
        last_code
    }

    /// Block until a specific PID completes. Returns exit code or -1 if not found.
    pub fn wait_pid(&mut self, pid: u64) -> i32 {
        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|j| j.pid == pid && j.status == JobStatus::Running)
        {
            let code = process::waitpid(job.pid);
            job.status = JobStatus::Done(code);
            self.jobs.retain(|j| j.status == JobStatus::Running);
            code
        } else if let Some(job) = self.jobs.iter().find(|j| j.pid == pid) {
            if let JobStatus::Done(code) = job.status {
                self.jobs.retain(|j| j.pid != pid);
                code
            } else {
                -1
            }
        } else {
            eprintln!("wait: no such job: {}", pid);
            -1
        }
    }

    /// Print all jobs.
    pub fn print(&self) {
        for job in &self.jobs {
            let status_str = match &job.status {
                JobStatus::Running => "Running".to_string(),
                JobStatus::Done(code) => format!("Done({})", code),
            };
            println!("[{}] {} {} {}", job.id, job.pid, status_str, job.command);
        }
    }
}
