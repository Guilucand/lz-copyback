//! Backend selection by file name, mirroring ggcat's `raw_reader::codec_of` and
//! `sequences_stream::tar::decoded_reader`.

use crate::backend::LzBackend;
use crate::backends::{DeflateBackend, Lz4Backend, Passthrough, XzBackend, ZstdBackend};
use crate::config::TrackerConfig;
use crate::decoder::TrackingDecoder;
use crate::error::Error;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Gzip,
    Xz,
    Zstd,
    Lz4,
    Bzip2,
    Plain,
}

pub fn codec_of(path: &Path) -> Codec {
    match path.extension().and_then(|e| e.to_str()) {
        Some("gz") | Some("tgz") => Codec::Gzip,
        Some("xz") | Some("txz") => Codec::Xz,
        Some("zst") | Some("zstd") => Codec::Zstd,
        Some("lz4") => Codec::Lz4,
        Some("bz2") | Some("tbz2") => Codec::Bzip2,
        _ => Codec::Plain,
    }
}

pub fn strip_compression_suffix(name: &str) -> &str {
    for suffix in [".gz", ".bz2", ".xz", ".zst", ".zstd", ".lz4"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped;
        }
    }
    name
}

/// Same rule as ggcat: `*.tar[.<codec>]`, `.tgz`, `.tbz2`, `.txz`.
pub fn is_archive(path: &Path) -> bool {
    path.to_str().is_some_and(|name| {
        strip_compression_suffix(name).ends_with(".tar")
            || [".tgz", ".tbz2", ".txz"].iter().any(|ext| name.ends_with(ext))
    })
}

pub type BoxedBackend = Box<dyn LzBackend + Send>;

/// Wrap `reader` in the backend for `codec`.
pub fn backend_for(reader: impl Read + Send + 'static, codec: Codec) -> std::io::Result<BoxedBackend> {
    Ok(match codec {
        Codec::Gzip => Box::new(DeflateBackend::new(reader)),
        Codec::Xz => Box::new(XzBackend::new(reader)?),
        Codec::Zstd => Box::new(ZstdBackend::new(reader)?),
        Codec::Lz4 => Box::new(Lz4Backend::new(reader)),
        Codec::Bzip2 => Box::new(Passthrough::new(bzip2::read::MultiBzDecoder::new(reader))),
        Codec::Plain => Box::new(Passthrough::new(reader)),
    })
}

/// Open a file, choosing the backend from its extension.
pub fn open(path: &Path, config: TrackerConfig) -> Result<TrackingDecoder<BoxedBackend>, Error> {
    let file = File::open(path)?;
    open_reader(BufReader::with_capacity(1 << 20, file), codec_of(path), config)
}

pub fn open_reader(
    reader: impl Read + Send + 'static,
    codec: Codec,
    config: TrackerConfig,
) -> Result<TrackingDecoder<BoxedBackend>, Error> {
    let backend = backend_for(reader, codec)?;
    Ok(TrackingDecoder::new(backend, config))
}
