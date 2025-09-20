use thiserror::Error;

#[derive(Debug, Error)]
pub enum VgaError {
    #[error("invalid device")]
    InvalidDevice,
}
