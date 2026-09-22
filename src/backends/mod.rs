//! Format-specific [`crate::LzBackend`] implementations.

pub mod deflate;
pub mod lz4;
pub mod passthrough;
pub mod xz;
pub mod zstd;

pub use deflate::DeflateBackend;
pub use lz4::Lz4Backend;
pub use passthrough::Passthrough;
pub use xz::XzBackend;
pub use zstd::ZstdBackend;
