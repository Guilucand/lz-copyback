//! Dump the members of tar archives together with the LZ copies that touch them.
//!
//! `analysis [--mode forward|backward] [--min-len N] [--threads N] [--max-window BYTES]
//!           [--min-window BYTES] [--self-overlap clip|drop] [--all-members]
//!           <file.tar[.gz|.xz|.zst|.lz4]>...`
//!
//! Every input is assumed to be a (possibly compressed) tar archive and is decoded on its
//! own thread with copy tracking on. Nothing is done with the output beyond writing it
//! out, grouped by the file *inside* the archive: one record per tar member holding its
//! content bytes and the copies attached to it. No annotation stream.
//!
//! The mode picks which end of a copy attaches it to a member: under `forward` (the
//! default) the copies whose *source* lies in it ("these bytes come back later"), under
//! `backward` the copies whose *destination* lies in it ("these bytes repeat something
//! earlier"). The backward direction loses no copy whatever its distance, while the
//! forward one can only report a copy while its source is still inside the tracking
//! window.
//!
//! Record format (a whole record is written under one stdout lock, so records of archives
//! decoded in parallel never interleave):
//!
//! ```text
//! #file archive=<path> member=<index> size=<bytes> copies=<n> opaque=<none|reason> dir=<mode> name=<name>
//! <size raw bytes><newline>
//! @ <src_off> <dst_member> <dst_off> <len>     (forward: the source is in this member)
//! @ <dst_off> <src_member> <src_off> <len>     (backward: the destination is in this member)
//! ```
//!
//! Offsets are member-relative. Copies touching headers, padding or opaque members
//! (directories, links, members that are themselves compressed) are dropped by the tar
//! layer, so opaque records carry no copies.
//!
//! A record is buffered in memory until its member ends, so peak usage is roughly
//! `threads * (largest member + window buffers)`.

use lz_copyback::open::open;
use lz_copyback::tar::{MemberCopy, MemberInfo, OpaqueReason, TarConfig, TarTracker};
use lz_copyback::{Direction, SelfOverlap, TrackerConfig};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Copies reported in the member holding their source.
    Forward,
    /// Copies reported in the member holding their destination.
    Backward,
}

struct Options {
    mode: Mode,
    config: TrackerConfig,
    /// Dump opaque members (directories, links, compressed members) too.
    all_members: bool,
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

fn opaque_name(r: Option<OpaqueReason>) -> &'static str {
    match r {
        None => "none",
        Some(OpaqueReason::NotRegularFile) => "not-regular-file",
        Some(OpaqueReason::CompressedName) => "compressed-name",
        Some(OpaqueReason::Filtered) => "filtered",
    }
}

/// The member currently being collected: its content bytes and the copies attached to it.
struct OpenMember {
    info: MemberInfo,
    /// False for a member that is not dumped: its bytes are counted, not kept.
    keep: bool,
    data: Vec<u8>,
    /// Content bytes seen so far, equal to `data.len()` when `keep`.
    seen: u64,
    copies: Vec<MemberCopy>,
}

static GLOBAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static TOT_GLOBAL_BYTES: AtomicUsize = AtomicUsize::new(0);

impl OpenMember {
    fn new(info: MemberInfo, keep: bool) -> Self {
        let cap = if keep {
            info.len().min(1 << 30) as usize
        } else {
            0
        };
        Self {
            info,
            keep,
            data: Vec::with_capacity(cap),
            seen: 0,
            copies: Vec::new(),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.keep {
            self.data.extend_from_slice(bytes);
        }
        self.seen += bytes.len() as u64;
    }

    /// Writes the finished record out under a single stdout lock, unless the member is
    /// one of the skipped ones.
    fn emit(self, archive: &str, mode: Mode, line: &mut Vec<u8>) -> std::io::Result<()> {
        if !self.keep {
            return Ok(());
        }
        let dir = if mode == Mode::Backward { "backward" } else { "forward" };
        line.clear();
        writeln!(
            line,
            "#file archive={archive} member={} size={} copies={} opaque={} dir={dir} name={}",
            self.info.index,
            self.seen,
            self.copies.len(),
            opaque_name(self.info.opaque),
            String::from_utf8_lossy(&self.info.name)
        )
        .unwrap();
        let header = line.len();
        for c in &self.copies {
            // The first two fields are the end that lies in this member.
            let (near, far_member, far) = match mode {
                Mode::Backward => (c.dst_off(), c.src_member, c.src_off()),
                _ => (c.src_off(), c.dst_member, c.dst_off()),
            };
            writeln!(line, "@ {} {} {} {}", near, far_member, far, c.len()).unwrap();
            // GLOBAL_BYTES.fetch_add(c.len() as usize, Ordering::Relaxed);
            GLOBAL_BYTES.fetch_add(
                c.len().saturating_sub(63 * 2 - 22) as usize,
                Ordering::Relaxed,
            );
        }
        let out = std::io::stdout();
        let mut out = out.lock();
        out.write_all(&line[..header])?;
        // out.write_all(&self.data)?;
        TOT_GLOBAL_BYTES.fetch_add(self.data.len(), Ordering::Relaxed);
        out.write_all(b"\n")?;
        out.write_all(&line[header..])?;
        Ok(())
    }
}

/// Decodes `path` as a tar archive and writes one record per member to stdout.
fn dump(path: &Path, opts: &Options) -> Result<(), String> {
    let archive = path.display().to_string();
    let mut dec = open(path, opts.config.clone()).map_err(|e| e.to_string())?;
    // Backward copies reach back by up to the stream's maximum distance, plus the window
    // still to be emitted, so their source member must stay in the table that long.
    let retain_back = match opts.mode {
        Mode::Backward => dec.max_distance().saturating_add(2 * dec.window_size() as u64),
        _ => 0,
    };
    let mut tar = TarTracker::new(TarConfig {
        min_len: opts.config.min_len,
        retain_back,
        ..TarConfig::default()
    });
    // Copies of the current window keyed by the member they are reported in (the source
    // member under `forward`, the destination member under `backward`), consumed as spans
    // of that member are appended. That member always has a span in this very window, so
    // nothing is lost.
    let mut pending: HashMap<u32, Vec<MemberCopy>> = HashMap::new();
    let mut open_member: Option<OpenMember> = None;
    let mut known: HashMap<u32, MemberInfo> = HashMap::new();
    let mut line = Vec::with_capacity(4 << 10);

    loop {
        let w = match dec.next_window() {
            Ok(Some(w)) => w,
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        };
        let tw = tar.process(&w).map_err(|e| format!("tar: {e}"))?;
        for m in tw.new_members {
            known.insert(m.index, m.clone());
        }
        for c in tw.copies {
            let member = if opts.mode == Mode::Backward { c.dst_member } else { c.src_member };
            pending.entry(member).or_default().push(*c);
        }
        for span in tw.spans {
            let cur = match open_member.as_mut() {
                Some(m) if m.info.index == span.member => m,
                _ => {
                    if let Some(done) = open_member.take() {
                        done.emit(&archive, opts.mode, &mut line).map_err(|e| e.to_string())?;
                    }
                    let info = known
                        .get(&span.member)
                        .ok_or_else(|| format!("unknown tar member {}", span.member))?
                        .clone();
                    let keep = opts.all_members || info.opaque.is_none();
                    open_member.insert(OpenMember::new(info, keep))
                }
            };
            if span.member_off != cur.seen {
                return Err(format!(
                    "member {} span out of order: at {}, span starts at {}",
                    cur.info.index, cur.seen, span.member_off
                ));
            }
            cur.push(&w.data[span.window_off..span.window_off + span.len]);
            if let Some(cs) = pending.remove(&cur.info.index) {
                cur.copies.extend(cs);
            }
        }
        // Copies of members whose content has already been fully collected but that are
        // still open (their last span arrived in an earlier window).
        if let Some(cur) = open_member.as_mut() {
            if let Some(cs) = pending.remove(&cur.info.index) {
                cur.copies.extend(cs);
            }
        }
        pending.retain(|_, v| !v.is_empty());
        if w.is_last {
            break;
        }
    }
    if let Some(done) = open_member.take() {
        done.emit(&archive, opts.mode, &mut line).map_err(|e| e.to_string())?;
    }
    for (member, copies) in &pending {
        eprintln!(
            "warning: {archive}: {} copies of member {member} arrived late",
            copies.len()
        );
    }
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut files: Vec<PathBuf> = Vec::new();
    let mut opts = Options {
        mode: Mode::Forward,
        config: TrackerConfig::default(),
        all_members: false,
    };
    let mut threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mode" => {
                opts.mode = match args.next().expect("value").as_str() {
                    "forward" => Mode::Forward,
                    "backward" => Mode::Backward,
                    other => panic!("bad --mode {other}"),
                }
            }
            "--min-len" => {
                opts.config.min_len = args.next().expect("value").parse().expect("bad --min-len")
            }
            "--threads" => threads = args.next().expect("value").parse().expect("bad --threads"),
            "--max-window" => opts.config.max_window = parse_size(&args.next().expect("value")),
            "--min-window" => opts.config.min_window = parse_size(&args.next().expect("value")),
            "--self-overlap" => {
                opts.config.self_overlap = match args.next().expect("value").as_str() {
                    "clip" => SelfOverlap::Clip,
                    "drop" => SelfOverlap::Drop,
                    other => panic!("bad --self-overlap {other}"),
                }
            }
            "--all-members" => opts.all_members = true,
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        eprintln!(
            "usage: analysis [--mode forward|backward] [--min-len N] [--threads N] [--max-window BYTES] [--min-window BYTES] [--self-overlap clip|drop] [--all-members] <archive>..."
        );
        std::process::exit(2);
    }
    opts.config.direction = match opts.mode {
        Mode::Backward => Direction::Backward,
        _ => Direction::Forward,
    };
    let threads = threads.clamp(1, files.len());

    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= files.len() {
                        break;
                    }
                    if let Err(e) = dump(&files[i], &opts) {
                        eprintln!("error: {}: {e}", files[i].display());
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    std::io::stdout().flush().expect("flush");
    println!(
        "Finished: {}/{} redundant bytes ({:.2}%)",
        GLOBAL_BYTES.load(Ordering::Relaxed),
        TOT_GLOBAL_BYTES.load(Ordering::Relaxed),
        GLOBAL_BYTES.load(Ordering::Relaxed) as f64
            / TOT_GLOBAL_BYTES.load(Ordering::Relaxed) as f64
            * 100.0
    );
    if failed.load(Ordering::Relaxed) > 0 {
        std::process::exit(1);
    }
}
