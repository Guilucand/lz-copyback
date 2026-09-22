//! Copyback statistics over compressed inputs (TODO point 1a).
//!
//! `lz_copyback_stats [--min-len N] [--threads N] [--max-window BYTES] [--self-overlap clip|drop] [--no-tar] <file>...`
//!
//! Decodes every file with copy tracking on, attributing copies to tar members when the
//! name looks like an archive, and prints Markdown tables: per file and in total, the
//! decompressed size, copies, copied bytes and their ratio, copies per MiB, the longest
//! copy, cross- versus intra-member counts, dropped counters and timing, plus a
//! power-of-two length histogram.

use lz_copyback::open::{is_archive, open};
use lz_copyback::tar::{TarConfig, TarStats, TarTracker};
use lz_copyback::{SelfOverlap, TrackerConfig, TrackerStats};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const HIST_BUCKETS: usize = 32;

#[derive(Default, Clone)]
struct FileReport {
    name: String,
    stats: TrackerStats,
    tar: Option<TarStats>,
    /// Bucket `i` counts copies with `62 * 2^i <= len < 62 * 2^(i+1)`.
    hist: [u64; HIST_BUCKETS],
    seconds: f64,
    error: Option<String>,
}

fn parse_size(s: &str) -> usize {
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1usize << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1usize << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1usize << 30),
        _ => (s, 1),
    };
    num.parse::<usize>().expect("bad size") * mult
}

fn bucket(len: u64, min_len: u64) -> usize {
    let ratio = (len / min_len.max(1)).max(1);
    ((63 - ratio.leading_zeros()) as usize).min(HIST_BUCKETS - 1)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn analyse(path: &Path, config: &TrackerConfig, use_tar: bool) -> FileReport {
    let start = Instant::now();
    let mut report = FileReport { name: path.display().to_string(), ..FileReport::default() };
    let mut dec = match open(path, config.clone()) {
        Ok(d) => d,
        Err(e) => {
            report.error = Some(e.to_string());
            return report;
        }
    };
    let mut tar = (use_tar && is_archive(path)).then(|| {
        TarTracker::new(TarConfig { min_len: config.min_len, ..TarConfig::default() })
    });
    loop {
        match dec.next_window() {
            Ok(Some(w)) => {
                for c in w.copies {
                    report.hist[bucket(c.len(), config.min_len)] += 1;
                }
                if let Some(t) = tar.as_mut() {
                    if let Err(e) = t.process(&w) {
                        report.error = Some(format!("tar: {e}"));
                        tar = None;
                    }
                }
                if w.is_last {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                report.error = Some(e.to_string());
                break;
            }
        }
    }
    report.stats = dec.stats();
    report.tar = tar.map(|t| t.stats());
    report.seconds = start.elapsed().as_secs_f64();
    report
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut files: Vec<PathBuf> = Vec::new();
    let mut config = TrackerConfig::default();
    let mut threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut use_tar = true;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--min-len" => config.min_len = args.next().expect("value").parse().expect("bad --min-len"),
            "--threads" => threads = args.next().expect("value").parse().expect("bad --threads"),
            "--max-window" => config.max_window = parse_size(&args.next().expect("value")),
            "--min-window" => config.min_window = parse_size(&args.next().expect("value")),
            "--self-overlap" => {
                config.self_overlap = match args.next().expect("value").as_str() {
                    "clip" => SelfOverlap::Clip,
                    "drop" => SelfOverlap::Drop,
                    other => panic!("bad --self-overlap {other}"),
                }
            }
            "--no-tar" => use_tar = false,
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        eprintln!("usage: lz_copyback_stats [--min-len N] [--threads N] [--max-window BYTES] [--self-overlap clip|drop] [--no-tar] <file>...");
        std::process::exit(2);
    }
    let threads = threads.clamp(1, files.len());
    let per_stream = 2 * config.max_window + lz_copyback::SLACK;
    eprintln!(
        "{} files, {threads} threads, min_len {}, window up to {} MiB (about {} MiB of window buffers)",
        files.len(),
        config.min_len,
        config.max_window >> 20,
        (threads * per_stream) >> 20
    );

    let next = AtomicUsize::new(0);
    let reports: Mutex<Vec<Option<FileReport>>> = Mutex::new(vec![None; files.len()]);
    let wall = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= files.len() {
                        break;
                    }
                    let r = analyse(&files[i], &config, use_tar);
                    eprintln!(
                        "done {} ({:.1} MiB out, {} copies, {:.1}s)",
                        r.name,
                        mib(r.stats.output_bytes.get()),
                        r.stats.emitted_copies.get(),
                        r.seconds
                    );
                    reports.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    let wall = wall.elapsed().as_secs_f64();
    let reports: Vec<FileReport> = reports.into_inner().unwrap().into_iter().map(|r| r.unwrap()).collect();

    println!("| file | out MiB | copies | copied MiB | copied/out | copies/MiB | longest | cross | intra | drop short/self/late/split | s |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|");
    let mut total = FileReport::default();
    let mut total_tar = TarStats::default();
    let mut any_tar = false;
    for r in &reports {
        let s = &r.stats;
        let (cross, intra) = r.tar.map_or(("-".to_string(), "-".to_string()), |t| {
            (t.cross_member.to_string(), t.intra_member.to_string())
        });
        println!(
            "| {} | {:.1} | {} | {:.1} | {:.4} | {:.2} | {} | {} | {} | {}/{}/{}/{} | {:.1} |",
            Path::new(&r.name).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(r.name.clone()),
            mib(s.output_bytes.get()),
            s.emitted_copies,
            mib(s.emitted_bytes.get()),
            s.emitted_bytes.get() as f64 / s.output_bytes.get().max(1) as f64,
            s.emitted_copies.get() as f64 / mib(s.output_bytes.get()).max(1e-9),
            s.longest_copy,
            cross,
            intra,
            s.dropped_short,
            s.dropped_self_overlap.get() + s.clipped_self_overlap.get(),
            s.dropped_late,
            s.dropped_split_short,
            r.seconds
        );
        if let Some(e) = &r.error {
            println!("| ⚠ {} | error: {e} |", r.name);
        }
        let t = &mut total.stats;
        t.raw_events.add(s.raw_events.get());
        t.merged_runs.add(s.merged_runs.get());
        t.dropped_short.add(s.dropped_short.get());
        t.dropped_self_overlap.add(s.dropped_self_overlap.get());
        t.clipped_self_overlap.add(s.clipped_self_overlap.get());
        t.dropped_late.add(s.dropped_late.get());
        t.over_window.add(s.over_window.get());
        t.dropped_split_short.add(s.dropped_split_short.get());
        t.emitted_copies.add(s.emitted_copies.get());
        t.emitted_bytes.add(s.emitted_bytes.get());
        t.longest_copy.max_with(s.longest_copy.get());
        t.output_bytes.add(s.output_bytes.get());
        t.windows.add(s.windows.get());
        for (a, b) in total.hist.iter_mut().zip(r.hist.iter()) {
            *a += b;
        }
        if let Some(ts) = r.tar {
            any_tar = true;
            total_tar.members.add(ts.members.get());
            total_tar.opaque_members.add(ts.opaque_members.get());
            total_tar.pieces.add(ts.pieces.get());
            total_tar.cross_member.add(ts.cross_member.get());
            total_tar.intra_member.add(ts.intra_member.get());
            total_tar.dropped_header_touch.add(ts.dropped_header_touch.get());
            total_tar.dropped_opaque.add(ts.dropped_opaque.get());
            total_tar.dropped_short_piece.add(ts.dropped_short_piece.get());
        }
    }
    let s = &total.stats;
    println!(
        "| **total** | {:.1} | {} | {:.1} | {:.4} | {:.2} | {} | {} | {} | {}/{}/{}/{} | {:.1} wall |",
        mib(s.output_bytes.get()),
        s.emitted_copies,
        mib(s.emitted_bytes.get()),
        s.emitted_bytes.get() as f64 / s.output_bytes.get().max(1) as f64,
        s.emitted_copies.get() as f64 / mib(s.output_bytes.get()).max(1e-9),
        s.longest_copy,
        if any_tar { total_tar.cross_member.to_string() } else { "-".into() },
        if any_tar { total_tar.intra_member.to_string() } else { "-".into() },
        s.dropped_short,
        s.dropped_self_overlap.get() + s.clipped_self_overlap.get(),
        s.dropped_late,
        s.dropped_split_short,
        wall
    );
    println!();
    println!("Raw match events: {}, merged runs: {}, over-window: {}", s.raw_events, s.merged_runs, s.over_window);
    if any_tar {
        println!(
            "Tar: {} members ({} opaque), {} pieces, dropped header-touch {}, opaque {}, short {}",
            total_tar.members,
            total_tar.opaque_members,
            total_tar.pieces,
            total_tar.dropped_header_touch,
            total_tar.dropped_opaque,
            total_tar.dropped_short_piece
        );
    }
    println!();
    println!("| copy length | copies | share |");
    println!("|---|---:|---:|");
    let total_copies = s.emitted_copies.get().max(1) as f64;
    for (i, n) in total.hist.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let lo = config.min_len << i;
        let hi = config.min_len << (i + 1);
        println!("| [{lo}, {hi}) | {n} | {:.2}% |", 100.0 * *n as f64 / total_copies);
    }
}
