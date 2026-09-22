//! Helpers shared by the integration tests.
#![allow(dead_code)]

use lz_copyback::{BackendError, CopyRecorder, LzBackend, LzCopy, TrackingDecoder};

/// A backend that replays a known output and fires scripted match events once the
/// bytes they describe have been "decoded".
pub struct ScriptedBackend {
    out: Vec<u8>,
    /// Maximum bytes produced per `decode` call (small values exercise short reads).
    chunk: usize,
    /// `(dst, distance, len)` in destination order.
    events: Vec<(u64, u64, u64)>,
    max_distance: u64,
    pos: usize,
    next_event: usize,
    /// Extra bytes written past `target` on the first call that reaches it.
    overshoot: usize,
    history: usize,
    finished: bool,
}

impl ScriptedBackend {
    pub fn new(out: Vec<u8>, events: Vec<(u64, u64, u64)>) -> Self {
        Self {
            out,
            chunk: usize::MAX,
            events,
            max_distance: 1 << 20,
            pos: 0,
            next_event: 0,
            overshoot: 0,
            history: 0,
            finished: false,
        }
    }

    pub fn chunk(mut self, chunk: usize) -> Self {
        self.chunk = chunk;
        self
    }

    pub fn max_distance(mut self, d: u64) -> Self {
        self.max_distance = d;
        self
    }

    pub fn overshoot(mut self, extra: usize) -> Self {
        self.overshoot = extra;
        self
    }

    pub fn history(mut self, history: usize) -> Self {
        self.history = history;
        self
    }
}

impl LzBackend for ScriptedBackend {
    fn max_distance(&self) -> u64 {
        self.max_distance
    }

    fn history_required(&self) -> usize {
        self.history
    }

    fn decode(
        &mut self,
        buf: &mut [u8],
        pos: usize,
        target: usize,
        rec: &mut CopyRecorder,
    ) -> Result<usize, BackendError> {
        assert!(!self.finished, "decode called after EOF");
        let remaining = self.out.len() - self.pos;
        let mut want = (target - pos).min(self.chunk);
        if self.overshoot > 0 && want == target - pos {
            want += self.overshoot.min(buf.len() - target);
            self.overshoot = 0;
        }
        let n = want.min(remaining);
        buf[pos..pos + n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        if n == 0 {
            self.finished = true;
        }
        while let Some(&(dst, dist, len)) = self.events.get(self.next_event) {
            if dst + len > self.pos as u64 {
                break;
            }
            rec.record(dst, dist, len);
            self.next_event += 1;
        }
        Ok(n)
    }
}

/// An owned snapshot of an emitted window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snap {
    pub index: u64,
    pub start: u64,
    pub data: Vec<u8>,
    pub lookahead_len: usize,
    pub copies: Vec<LzCopy>,
    pub is_last: bool,
}

/// Drain a decoder, checking the structural invariants of every window against the
/// reference output, and return the snapshots.
pub fn drain<B: LzBackend>(dec: &mut TrackingDecoder<B>, reference: &[u8], min_len: u64) -> Vec<Snap> {
    let mut snaps = Vec::new();
    let mut next_start = 0u64;
    let mut seen_last = false;
    let window_size = dec.window_size();
    while let Some(w) = dec.next_window().expect("decode error") {
        assert!(!seen_last, "window after the last one");
        assert_eq!(w.start_abs, next_start, "windows are contiguous");
        assert_eq!(
            reference[w.start_abs as usize..w.start_abs as usize + w.data.len()],
            *w.data,
            "window bytes equal the reference"
        );
        let end = w.start_abs + w.data.len() as u64;
        assert_eq!(
            reference[end as usize..end as usize + w.lookahead.len()],
            *w.lookahead,
            "lookahead bytes equal the reference"
        );
        if !w.is_last {
            assert_eq!(w.data.len(), window_size, "non-last windows are full");
        } else {
            assert!(w.lookahead.is_empty(), "the last window has no lookahead");
        }
        let decoded_end = end + w.lookahead.len() as u64;
        for pair in w.copies.windows(2) {
            assert!((pair[0].src(), pair[0].dst()) <= (pair[1].src(), pair[1].dst()), "copies sorted");
        }
        for c in w.copies {
            assert!(c.src() >= w.start_abs && c.src_end() <= end, "source inside window: {c:?}");
            assert!(c.dst() > c.src() && c.distance() >= c.len(), "non-overlapping forward copy: {c:?}");
            assert!(c.len() >= min_len, "minimum length: {c:?}");
            assert!(c.dst_end() <= decoded_end, "destination decoded: {c:?}");
            assert_eq!(
                reference[c.src() as usize..c.src_end() as usize],
                reference[c.dst() as usize..c.dst_end() as usize],
                "copy bytes match: {c:?}"
            );
        }
        seen_last = w.is_last;
        next_start = end;
        snaps.push(Snap {
            index: w.index,
            start: w.start_abs,
            data: w.data.to_vec(),
            lookahead_len: w.lookahead.len(),
            copies: w.copies.to_vec(),
            is_last: w.is_last,
        });
    }
    assert!(seen_last, "exactly one last window");
    assert_eq!(next_start as usize, reference.len(), "all bytes emitted");
    snaps
}

/// Pseudo-random bytes (a fixed LCG, no dependencies) so scripted copies are checkable
/// by the byte-equality assertion: `plant` makes the destination equal to the source.
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// Make `data[dst..dst + len]` a copy of `data[src..src + len]` (`dst >= src + len`).
pub fn plant(data: &mut [u8], src: usize, dst: usize, len: usize) {
    assert!(dst >= src + len);
    let (a, b) = data.split_at_mut(dst);
    b[..len].copy_from_slice(&a[src..src + len]);
}

/// Scripted data with every event's destination bytes made equal to its source.
pub fn scripted(len: usize, events: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut data = random_bytes(len as u64, len);
    for &(dst, dist, l) in events {
        let l = l.min(dist) as usize; // self-overlapping events are periodic: plant the prefix
        if l > 0 {
            plant(&mut data, (dst - dist) as usize, dst as usize, l);
            if (l as u64) < dist.min(l as u64) {
                unreachable!()
            }
        }
    }
    data
}

// ---------------------------------------------------------------------------------------
// Realistic fixtures: FASTA-like data with repeats, encoders, reference decoders.

use rand::{Rng, SeedableRng, rngs::StdRng};
use std::io::{Read, Write};

/// Random ACGT records of `record_len` bases wrapped at 80 columns. With probability
/// `repeat_prob` a record is a verbatim copy of one of the previous `back` records, so the
/// data has long repeats at distances bounded by `back * record_len`.
pub fn fasta(seed: u64, records: usize, record_len: usize, repeat_prob: f64, back: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new();
    for i in 0..records {
        let header = format!(">r{i}\n");
        let body_start = out.len() + header.len();
        if !starts.is_empty() && rng.gen_bool(repeat_prob) {
            let lo = starts.len().saturating_sub(back);
            let (s, e) = starts[rng.gen_range(lo..starts.len())];
            let copy = out[s..e].to_vec();
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(&copy);
        } else {
            out.extend_from_slice(header.as_bytes());
            for col in 0..record_len {
                out.push(b"ACGT"[rng.gen_range(0..4)]);
                if col % 80 == 79 || col + 1 == record_len {
                    out.push(b'\n');
                }
            }
        }
        starts.push((body_start, out.len()));
    }
    out
}

/// Does some copy reproduce the planted repeat `(src, dst, len)`: same distance and
/// covering at least `min_cover` of the planted source range?
pub fn covers(copies: &[LzCopy], src: u64, dst: u64, len: u64, min_cover: f64) -> bool {
    copies.iter().any(|c| {
        c.distance() == dst - src && {
            let lo = c.src().max(src);
            let hi = c.src_end().min(src + len);
            hi > lo && (hi - lo) as f64 >= min_cover * len as f64
        }
    })
}

pub fn xz_encode(data: &[u8], dict_size: u32) -> Vec<u8> {
    use liblzma::stream::{Check, Filters, LzmaOptions, Stream};
    let mut opts = LzmaOptions::new_preset(6).unwrap();
    opts.dict_size(dict_size);
    let mut filters = Filters::new();
    filters.lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::Crc64).unwrap();
    let mut enc = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// An xz stream with an x86 BCJ filter in front of LZMA2.
pub fn xz_encode_bcj(data: &[u8]) -> Vec<u8> {
    use liblzma::stream::{Check, Filters, LzmaOptions, Stream};
    let opts = LzmaOptions::new_preset(6).unwrap();
    let mut filters = Filters::new();
    filters.x86().lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::Crc64).unwrap();
    let mut enc = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn xz_reference(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    liblzma::read::XzDecoder::new_multi_decoder(bytes).read_to_end(&mut out).unwrap();
    out
}

pub fn gzip_encode(data: &[u8], level: u32) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn gzip_reference(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes).read_to_end(&mut out).unwrap();
    out
}

/// Run a decoder to completion, returning the error if any (used for negative tests).
pub fn drain_result<B: LzBackend>(dec: &mut TrackingDecoder<B>) -> Result<Vec<u8>, lz_copyback::Error> {
    let mut out = Vec::new();
    loop {
        match dec.next_window()? {
            Some(w) => {
                out.extend_from_slice(w.data);
                if w.is_last {
                    return Ok(out);
                }
            }
            None => return Ok(out),
        }
    }
}

pub fn zstd_encode(data: &[u8], window_log: u32) -> Vec<u8> {
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    enc.window_log(window_log).unwrap();
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn zstd_reference(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut dec = zstd::stream::read::Decoder::new(bytes).unwrap();
    dec.read_to_end(&mut out).unwrap();
    out
}

pub fn lz4_encode(data: &[u8], linked: bool) -> Vec<u8> {
    let mode = if linked { lz4::BlockMode::Linked } else { lz4::BlockMode::Independent };
    let mut enc = lz4::EncoderBuilder::new().level(4).block_mode(mode).build(Vec::new()).unwrap();
    enc.write_all(data).unwrap();
    let (out, result) = enc.finish();
    result.unwrap();
    out
}

pub fn lz4_reference(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lz4::Decoder::new(bytes).unwrap().read_to_end(&mut out).unwrap();
    out
}

// ---------------------------------------------------------------------------------------
// Tar fixtures.

/// A GNU tar with the given regular-file members (long names get an 'L' entry
/// automatically) and the usual two zero blocks at the end.
pub fn tar_bytes(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

/// A single 512-byte block, zero padded.
pub fn tar_block(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    b.resize(b.len().div_ceil(512) * 512, 0);
    b
}

/// A raw ustar member with an explicit typeflag and size field.
pub fn tar_member_raw(name: &str, typeflag: u8, size_field: Option<u64>, data: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_ustar();
    header.set_path(name).unwrap();
    header.set_mode(0o644);
    header.as_mut_bytes()[156] = typeflag;
    if let Some(size) = size_field {
        header.set_size(size);
    }
    header.set_cksum();
    let mut out = header.as_bytes().to_vec();
    out.extend_from_slice(&tar_block(data));
    out
}

/// A regular member whose size is stored in GNU base-256 form.
pub fn tar_member_base256(name: &str, data: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_path(name).unwrap();
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    {
        let size = header.as_mut_bytes();
        let field = &mut size[124..136];
        field.fill(0);
        field[0] = 0x80;
        field[4..12].copy_from_slice(&(data.len() as u64).to_be_bytes());
    }
    header.set_cksum();
    let mut out = header.as_bytes().to_vec();
    out.extend_from_slice(&tar_block(data));
    out
}

/// A PAX 'x' extended header carrying `path` and `size`, followed by a ustar header with
/// a short name and a zero size, then the data.
pub fn tar_member_pax(long_name: &str, data: &[u8]) -> Vec<u8> {
    fn record(key: &str, value: &str) -> String {
        // "<len> key=value\n" where len counts the whole record including itself.
        let base = key.len() + value.len() + 3;
        let mut len = base + base.to_string().len();
        if len.to_string().len() + base != len {
            len = base + len.to_string().len();
        }
        format!("{len} {key}={value}\n")
    }
    let records = format!("{}{}", record("path", long_name), record("size", &data.len().to_string()));
    let mut pax = tar::Header::new_ustar();
    pax.set_path("PaxHeaders/x").unwrap();
    pax.set_entry_type(tar::EntryType::XHeader);
    pax.set_size(records.len() as u64);
    pax.set_mode(0o644);
    pax.set_cksum();
    let mut out = pax.as_bytes().to_vec();
    out.extend_from_slice(&tar_block(records.as_bytes()));
    let mut header = tar::Header::new_ustar();
    header.set_path("short").unwrap();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&tar_block(data));
    out
}

pub fn tar_end() -> Vec<u8> {
    vec![0u8; 1024]
}

/// A unique temporary path with the given file name suffix.
pub fn temp_path(suffix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("lz_copyback_{}_{n}_{suffix}", std::process::id()))
}
