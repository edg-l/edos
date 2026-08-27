use crate::report::Report;

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsckExitCode {
    Clean = 0,
    ErrorsFixed = 1,
    ErrorsRemain = 4,
    OperationalError = 8,
    UsageError = 16,
}

impl FsckExitCode {
    pub fn code(self) -> i32 {
        self as i32
    }
}

impl From<&Report> for FsckExitCode {
    fn from(report: &Report) -> Self {
        if report.has_errors() {
            FsckExitCode::ErrorsRemain
        } else {
            FsckExitCode::Clean
        }
    }
}
