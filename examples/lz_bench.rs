//! Decode-speed micro-benchmark for the hook overhead measurement.
//!
//! `lz_bench <file> [--limit-bytes N] [--mode reference|flate2|off|on]`
//!
//! * `reference` reads the file through the plain reader ggcat uses for that format today
//!   (streaming-libdeflate for `.gz`, liblzma for `.xz`, the zstd crate for `.zst`, the lz4
//!   C binding for `.lz4`) into a 1 MiB buffer and discards the bytes.
//! * `flate2` reads a `.gz` through `flate2::MultiGzDecoder`, ggcat's tar-path gzip reader.
//! * `off` / `on` drive the tracking decoder with copy tracking disabled / enabled.
//!
//! Prints elapsed seconds, output MiB/s and, for `on`, the copies found.

use lz_copyback::{Codec, codec_of};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn parse_size(s: &str) -> u64 {
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1u64 << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().expect("bad size") * mult
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut file: Option<PathBuf> = None;
    let mut limit = u64::MAX;
    let mut mode = String::from("reference");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--limit-bytes" => limit = parse_size(&args.next().expect("--limit-bytes needs a value")),
            "--mode" => mode = args.next().expect("--mode needs a value"),
            _ => file = Some(PathBuf::from(a)),
        }
    }
    let file = file.expect("usage: lz_bench <file> [--limit-bytes N] [--mode reference|flate2|off|on]");

    let start = Instant::now();
    let (total, copies, copied, raw) = match mode.as_str() {
        "reference" => (reference(&file, limit), 0, 0, 0),
        "flate2" => (drain(flate2::read::MultiGzDecoder::new(open(&file)), limit), 0, 0, 0),
        "off" => tracked(&file, limit, false),
        "on" => tracked(&file, limit, true),
        other => panic!("unknown mode {other}"),
    };
    let secs = start.elapsed().as_secs_f64();
    println!(
        "mode={mode} bytes={total} secs={secs:.3} MiB/s={:.1} copies={copies} copied_bytes={copied} raw_events={raw}",
        total as f64 / (1024.0 * 1024.0) / secs
    );
}

fn open(file: &Path) -> std::io::BufReader<std::fs::File> {
    std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(file).expect("open"))
}

fn drain(mut reader: impl Read, limit: u64) -> u64 {
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    while total < limit {
        let n = reader.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    total
}

fn reference(file: &Path, limit: u64) -> u64 {
    match codec_of(file) {
        Codec::Gzip => {
            // ggcat's loose-file gzip path (crates/io/src/raw_reader.rs).
            let mut total = 0u64;
            streaming_libdeflate_rs::decompress_file_buffered_callback(
                file,
                |data| {
                    total += data.len() as u64;
                    if total >= limit { Err(()) } else { Ok(()) }
                },
                4 << 20,
            )
            .ok();
            total
        }
        Codec::Xz => drain(liblzma::read::XzDecoder::new_multi_decoder(open(file)), limit),
        Codec::Zstd => drain(zstd::stream::read::Decoder::new(open(file)).expect("zstd"), limit),
        Codec::Lz4 => drain(lz4::Decoder::new(open(file)).expect("lz4"), limit),
        Codec::Bzip2 => drain(bzip2::read::MultiBzDecoder::new(open(file)), limit),
        Codec::Plain => drain(open(file), limit),
    }
}

fn tracked(file: &Path, limit: u64, on: bool) -> (u64, u64, u64, u64) {
    use lz_copyback::TrackerConfig;
    let config = TrackerConfig { tracking: on, ..TrackerConfig::default() };
    let mut dec = lz_copyback::open::open(file, config).expect("open");
    let mut total = 0u64;
    while total < limit {
        match dec.next_window().expect("decode") {
            Some(w) => total += w.data.len() as u64,
            None => break,
        }
    }
    let stats = dec.stats();
    (total, stats.emitted_copies.get(), stats.emitted_bytes.get(), stats.raw_events.get())
}
