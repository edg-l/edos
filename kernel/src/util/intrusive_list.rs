//! Re-export from the shared `intrusive-list` crate.
//!
//! The implementation lives in `libs/intrusive_list/` so that it can be
//! tested with a standard `cargo test` runner outside the kernel.

pub use intrusive_list::*;
