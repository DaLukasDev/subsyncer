//! subsyncer-core: subtitle parsing, alignment and scoring.
//!
//! Pure library: no file system, no subprocesses. All timelines are boolean
//! arrays on a 10 ms grid (see [`timeline`]).

pub mod encoding;
pub mod subtitle;
pub mod timeline;

pub use timeline::{Interval, Timeline};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("encoding error: {0}")]
    Encoding(String),
}
