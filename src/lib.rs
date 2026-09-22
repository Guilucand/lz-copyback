//! Streaming decompression that reports the LZ77 copies the decoder executes.
//!
//! Every LZ-based format (deflate, LZMA2, zstd, lz4) produces most of its output by
//! copying bytes that appeared earlier in the stream. A [`TrackingDecoder`] wraps a
//! format-specific [`LzBackend`], keeps a window of twice the maximum copy distance and
//! hands the output out one half-window at a time together with its copies. Under
//! [`Direction::Forward`] those are the copies whose source lies in that half: byte
//! ranges that will appear again, verbatim, later in the stream. Under
//! [`Direction::Backward`] they are the copies whose destination lies in it: ranges that
//! repeat bytes from anywhere earlier in the stream. Copies are merged when the
//! compressor split them, clipped so they never overlap their own source, and filtered by
//! a minimum length.
//!
//! The [`tar`] module maps those copies onto the members of a tar archive, so a copy from
//! one file to another inside a `.tar.xz` is attributed to both members.
//!
//! Formats without an LZ stage (bzip2, plain files) go through a passthrough backend that
//! reports no copies, so consumers have a single code path.

pub mod backend;
pub mod backends;
pub mod config;
pub mod copy;
pub mod decoder;
pub mod error;
pub mod open;
pub mod recorder;
mod sort;
pub mod stats;
pub mod tar;

pub use backend::{BackendError, LzBackend, SLACK};
pub use config::{Direction, SelfOverlap, TrackerConfig};
pub use copy::LzCopy;
pub use decoder::{TrackingDecoder, Window};
pub use error::Error;
pub use open::{Codec, codec_of, is_archive, open, open_reader};
pub use recorder::CopyRecorder;
pub use stats::{Stat, TrackerStats};
