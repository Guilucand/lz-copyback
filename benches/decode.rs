//! Decoding throughput of each decoder with copyback tracking off and on, next to the stock
//! decoder of its format. Every configuration decodes its input once, from memory. The stock
//! decoders are read `--read-size` bytes per call; the decoders here as a consumer of chunks
//! reads them: `--read-size` bytes at a time, decoding `--window` bytes past each chunk first,
//! and emptying the copyback queue after each.
//!
//! ```text
//! cargo bench --bench decode -- [--synthetic-mib N] [--read-size BYTES] [--window BYTES]
//!                               [--real FILE.xz --scratch DIR] [--threads N] [--pause SECONDS]
//! ```
//!
//! With `--threads`, every configuration runs on that many threads at once, each decoding the
//! same input, started together; the throughput is all their bytes over the wall time.
//! `--pause` idles that long before each configuration, so that the clocks recover.
//!
//! The synthetic input is a batch of near-identical genomes (long repeats one genome apart),
//! encoded with each codec. `--real` adds an `.xz` file as shipped, plus its content
//! recompressed to gzip, zstd and lz4; the recompressed files are cached in `DIR`.

use lz_copyback::InPlaceDecoder;
use std::hint::black_box;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    Xz,
    Zstd,
    Lz4,
    Gzip,
}

struct Input {
    name: String,
    codec: Codec,
    encoded: Vec<u8>,
    decoded_len: u64,
}

#[derive(Clone, Copy)]
enum Config {
    Stock,
    Off,
    On(u32),
}

impl Config {
    fn label(self) -> String {
        match self {
            Config::Stock => "stock".to_string(),
            Config::Off => "tracking off".to_string(),
            Config::On(min_len) => format!("tracking on, min {min_len}"),
        }
    }
}

#[derive(Default)]
struct Run {
    time: Duration,
    bytes: u64,
    copybacks: u64,
    covered: u64,
    max_queue: usize,
}

/// Reads a stock decoder to the end in `read_size` calls.
fn run(mut reader: impl Read, read_size: usize) -> Run {
    let mut buffer = vec![0u8; read_size];
    let mut result = Run::default();
    let start = Instant::now();
    loop {
        let count = reader.read(&mut buffer).expect("decode error");
        if count == 0 {
            break;
        }
        result.bytes += count as u64;
    }
    result.time = start.elapsed();
    result
}

/// Reads `decoder` to the end a chunk of `chunk` bytes at a time, decoding `window` bytes past
/// each first, and taking the chunk's pieces and the copybacks queued so far.
fn run_in_place(mut decoder: Box<dyn InPlaceDecoder + '_>, chunk: usize, window: usize) -> Run {
    let mut result = Run::default();
    let start = Instant::now();
    let mut position = 0;
    loop {
        let decoded = decoder.decode_to(position + (chunk + window) as u64).expect("decode error");
        let end = decoded.min(position + chunk as u64);
        let (pieces, queue) = decoder.bytes_and_copybacks(position..end);
        for piece in pieces {
            black_box(piece);
        }
        result.max_queue = result.max_queue.max(queue.len());
        result.copybacks += queue.len() as u64;
        result.covered += queue.drain(..).map(|c| c.len as u64).sum::<u64>();
        if end == position {
            break;
        }
        result.bytes += end - position;
        position = end;
    }
    result.time = start.elapsed();
    result
}

fn bench(input: &Input, config: Config, read_size: usize, window: usize) -> Run {
    let encoded = &input.encoded[..];
    let (tracking, min_len) = match config {
        Config::Stock => {
            let result = match input.codec {
                Codec::Xz => run(liblzma::read::XzDecoder::new_multi_decoder(encoded), read_size),
                Codec::Zstd => run(zstd::stream::read::Decoder::new(encoded).unwrap(), read_size),
                Codec::Lz4 => run(lz4::Decoder::new(encoded).unwrap(), read_size),
                Codec::Gzip => run(StockGzip::new(encoded, read_size), read_size),
            };
            assert_eq!(result.bytes, input.decoded_len, "{} stock: wrong output size", input.name);
            return result;
        }
        Config::Off => (false, 1),
        Config::On(min_len) => (true, min_len),
    };
    let retain = read_size + window;
    // The hard threshold at the soft one, which is set below: the decoders filter at min_len.
    let mut decoder: Box<dyn InPlaceDecoder> = match input.codec {
        Codec::Xz => Box::new(lz_copyback::xz::XzInPlaceDecoder::new_multi_decoder(encoded, retain, u32::MAX)),
        Codec::Zstd => Box::new(lz_copyback::zstd::ZstdInPlaceDecoder::new(encoded, retain, u32::MAX).unwrap()),
        Codec::Lz4 => Box::new(lz_copyback::lz4::Lz4InPlaceDecoder::new(encoded, retain, u32::MAX).unwrap()),
        Codec::Gzip => Box::new(lz_copyback::gzip::GzipInPlaceDecoder::new(encoded, retain, u32::MAX)),
    };
    decoder.set_copybacks_tracking(tracking);
    decoder.set_copybacks_min_len(min_len);
    let result = run_in_place(decoder, read_size, window);
    assert_eq!(result.bytes, input.decoded_len, "{} {:?}: wrong output size", input.name, config.label());
    result
}

/// Runs `config` on `threads` threads at once, each decoding the whole input, and returns the
/// wall time from their common start to the last one's end, with each thread's run.
fn bench_threads(input: &Input, config: Config, read_size: usize, window: usize, threads: usize) -> (Duration, Vec<Run>) {
    let barrier = Barrier::new(threads + 1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    bench(input, config, read_size, window)
                })
            })
            .collect();
        barrier.wait();
        let start = Instant::now();
        let runs: Vec<Run> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
        (start.elapsed(), runs)
    })
}

/// The mean clock of the cores, where Linux tells it.
fn mean_mhz() -> Option<f64> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let clocks: Vec<f64> = cpuinfo
        .lines()
        .filter(|line| line.starts_with("cpu MHz"))
        .filter_map(|line| line.split(':').nth(1)?.trim().parse().ok())
        .collect();
    (!clocks.is_empty()).then(|| clocks.iter().sum::<f64>() / clocks.len() as f64)
}

/// streaming-libdeflate-rs's own push decompressor, as a `Read`.
struct StockGzip<'a> {
    input: &'a [u8],
    decompressor: streaming_libdeflate_rs::LibdeflateGzipPushDecompressor,
    pending: usize,
}

impl<'a> StockGzip<'a> {
    fn new(input: &'a [u8], buf_size: usize) -> Self {
        let decompressor = streaming_libdeflate_rs::LibdeflateGzipPushDecompressor::new(buf_size);
        Self { input, decompressor, pending: 0 }
    }
}

impl Read for StockGzip<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        use streaming_libdeflate_rs::LibdeflateDecompressResult as Result;
        while self.pending == self.decompressor.pending_output().len() {
            self.decompressor.consume_output(self.pending);
            self.pending = 0;
            match self.decompressor.decompress().unwrap() {
                Result::EndOfStream => return Ok(0),
                Result::NeedInput => {
                    let space = self.decompressor.input_space_mut();
                    let count = space.len().min(self.input.len());
                    if count == 0 {
                        self.decompressor.finish_input();
                    } else {
                        space[..count].copy_from_slice(&self.input[..count]);
                        self.input = &self.input[count..];
                        self.decompressor.commit_input(count);
                    }
                }
                Result::MoreData => {}
            }
        }
        let pending = &self.decompressor.pending_output()[self.pending..];
        let count = pending.len().min(buffer.len());
        buffer[..count].copy_from_slice(&pending[..count]);
        self.pending += count;
        Ok(count)
    }
}

/// `xz -6`, `zstd -3`, `gzip -6`, and lz4's command line defaults (level 1, 4 MiB
/// independent blocks, content checksum).
fn encode(codec: Codec, data: &[u8]) -> Vec<u8> {
    match codec {
        Codec::Xz => {
            let mut encoder = liblzma::write::XzEncoder::new(Vec::new(), 6);
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        }
        Codec::Zstd => zstd::encode_all(data, 3).unwrap(),
        Codec::Lz4 => {
            let mut encoder = lz4::EncoderBuilder::new()
                .level(1)
                .block_size(lz4::BlockSize::Max4MB)
                .block_mode(lz4::BlockMode::Independent)
                .checksum(lz4::ContentChecksum::ChecksumEnabled)
                .build(Vec::new())
                .unwrap();
            encoder.write_all(data).unwrap();
            let (out, result) = encoder.finish();
            result.unwrap();
            out
        }
        Codec::Gzip => {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        }
    }
}

/// A batch of near-identical genomes as FASTA: a random 1 MiB base genome, then copies of it
/// with a substitution every ~1000 bases and a short insertion every ~50 000, wrapped at 80
/// columns. Most of the batch repeats a genome one or a few genomes back.
fn synthetic(size: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        state >> 33
    };
    const GENOME: usize = 1 << 20;
    let base: Vec<u8> = (0..GENOME).map(|_| b"ACGT"[(next() % 4) as usize]).collect();
    let mut out = Vec::with_capacity(size + GENOME);
    let mut genome = 0;
    while out.len() < size {
        let mut sequence = Vec::with_capacity(GENOME + 1024);
        for &base in &base {
            match next() % 50_000 {
                0..=49 => sequence.push(b"ACGT"[(next() % 4) as usize]),
                50 => sequence.extend((0..1 + next() % 20).map(|_| b"ACGT"[(next() % 4) as usize])),
                _ => sequence.push(base),
            }
        }
        out.extend_from_slice(format!(">genome_{genome}\n").as_bytes());
        for line in sequence.chunks(80) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        genome += 1;
    }
    out
}

fn synthetic_inputs(mib: usize) -> Vec<Input> {
    let data = synthetic(mib << 20);
    [Codec::Xz, Codec::Zstd, Codec::Lz4, Codec::Gzip]
        .into_iter()
        .map(|codec| Input {
            name: "synthetic".to_string(),
            codec,
            encoded: encode(codec, &data),
            decoded_len: data.len() as u64,
        })
        .collect()
}

/// `path` as shipped, and its content recompressed to the other codecs, cached in `scratch`.
fn real_inputs(path: &Path, scratch: &Path) -> Vec<Input> {
    let name = path.file_name().unwrap().to_string_lossy().trim_end_matches(".xz").to_string();
    let shipped = std::fs::read(path).unwrap();
    let mut decoded: Option<Vec<u8>> = None;
    let mut decoded_len = None;
    let mut inputs = Vec::new();
    for (codec, extension) in [(Codec::Xz, ""), (Codec::Zstd, "zst"), (Codec::Lz4, "lz4"), (Codec::Gzip, "gz")] {
        let encoded = if codec == Codec::Xz {
            shipped.clone()
        } else {
            let cached: PathBuf = scratch.join(format!("{name}.{extension}"));
            if cached.exists() {
                std::fs::read(&cached).unwrap()
            } else {
                let data = decoded.get_or_insert_with(|| {
                    let mut out = Vec::new();
                    liblzma::read::XzDecoder::new_multi_decoder(&shipped[..]).read_to_end(&mut out).unwrap();
                    out
                });
                let encoded = encode(codec, data);
                std::fs::create_dir_all(scratch).unwrap();
                std::fs::write(&cached, &encoded).unwrap();
                encoded
            }
        };
        let len = *decoded_len.get_or_insert_with(|| {
            decoded.as_ref().map(|d| d.len() as u64).unwrap_or_else(|| {
                let mut sink = std::io::sink();
                std::io::copy(&mut liblzma::read::XzDecoder::new_multi_decoder(&shipped[..]), &mut sink).unwrap()
            })
        });
        inputs.push(Input { name: name.clone(), codec, encoded, decoded_len: len });
    }
    inputs
}

fn main() {
    let mut synthetic_mib = 256;
    let mut read_size = 4 << 20;
    let mut window = 1 << 20;
    let mut threads = 1;
    let mut pause = 0.0;
    let mut real = None;
    let mut scratch = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--synthetic-mib" => synthetic_mib = args.next().unwrap().parse().unwrap(),
            "--read-size" => read_size = args.next().unwrap().parse().unwrap(),
            "--window" => window = args.next().unwrap().parse().unwrap(),
            "--threads" => threads = args.next().unwrap().parse().unwrap(),
            "--pause" => pause = args.next().unwrap().parse().unwrap(),
            "--real" => real = Some(PathBuf::from(args.next().unwrap())),
            "--scratch" => scratch = Some(PathBuf::from(args.next().unwrap())),
            "--bench" => {} // passed by `cargo bench`
            other => panic!("unknown argument {other}"),
        }
    }

    let mut inputs = Vec::new();
    if synthetic_mib > 0 {
        inputs.extend(synthetic_inputs(synthetic_mib));
    }
    if let Some(real) = real {
        let scratch = scratch.expect("--real needs --scratch DIR for the recompressed copies");
        inputs.extend(real_inputs(&real, &scratch));
    }

    println!(
        "read size {read_size} bytes, in-place window {window} bytes, {threads} threads, {pause} s pause; every configuration decodes its input once per thread"
    );
    for input in &inputs {
        println!(
            "\n## {} {:?}: {} bytes -> {} bytes",
            input.name, input.codec, input.encoded.len(), input.decoded_len
        );
        println!(
            "| configuration | wall seconds | MB/s | MB/s per thread | vs stock | copybacks per thread | bytes covered | max queue | MHz before |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
        let mut stock = None;
        let configs = [Config::Stock, Config::Off, Config::On(62), Config::On(1)];
        for config in configs {
            std::thread::sleep(Duration::from_secs_f64(pause));
            let mhz = mean_mhz().map_or("-".to_string(), |mhz| format!("{mhz:.0}"));
            let (wall, runs) = bench_threads(input, config, read_size, window, threads);
            let seconds = wall.as_secs_f64();
            let stock_seconds = *stock.get_or_insert(seconds);
            let bytes: u64 = runs.iter().map(|run| run.bytes).sum();
            let per_thread = runs.iter().map(|run| run.bytes as f64 / run.time.as_secs_f64()).sum::<f64>() / runs.len() as f64;
            let first = &runs[0];
            println!(
                "| {} | {:.3} | {:.0} | {:.0} | {:+.1}% | {} | {:.1}% | {} | {} |",
                config.label(),
                seconds,
                bytes as f64 / seconds / 1e6,
                per_thread / 1e6,
                (seconds / stock_seconds - 1.0) * 100.0,
                first.copybacks,
                first.covered as f64 / first.bytes as f64 * 100.0,
                runs.iter().map(|run| run.max_queue).max().unwrap(),
                mhz,
            );
        }
    }
}
