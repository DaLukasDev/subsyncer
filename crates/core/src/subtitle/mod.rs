//! Subtitle parsing and timestamp-only rewriting for SRT, WebVTT and ASS/SSA.
//!
//! Parsers record the byte span of every timestamp in the file. Rendering
//! replaces only those spans; everything else is copied verbatim, so an
//! unshifted file renders byte-identical to its input.

pub mod timestamp;

pub use timestamp::TsStyle;
