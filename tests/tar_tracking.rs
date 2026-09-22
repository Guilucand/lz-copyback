//! Copies over a tar stream must be attributed to the right members with member-relative
//! offsets, split at member boundaries, never touch headers or opaque members, and the
//! header parser must handle GNU long names, PAX headers, base-256 sizes, window-straddling
//! headers and truncation.

mod common;

use common::{
    ScriptedBackend, drain, fasta, plant, random_bytes, tar_block, tar_bytes, tar_end, tar_member_base256,
    tar_member_pax, tar_member_raw, temp_path, xz_encode,
};
use lz_copyback::tar::{MemberCopy, OpaqueReason, TarConfig, TarError, TarTracker};
use lz_copyback::{LzCopy, TrackerConfig, TrackingDecoder, Window};

const W: usize = 1024;

/// Run a scripted tar stream through the tracker, collecting per-window results.
struct Run {
    windows: Vec<(Vec<MemberCopy>, Vec<lz_copyback::tar::MemberSpan>)>,
    members: Vec<lz_copyback::tar::MemberInfo>,
    stats: lz_copyback::tar::TarStats,
    error: Option<TarError>,
}

fn run(tar: &[u8], events: &[(u64, u64, u64)], window: usize, min_len: u64, config: TarConfig) -> Run {
    let mut data = tar.to_vec();
    for &(dst, dist, len) in events {
        plant(&mut data, (dst - dist) as usize, dst as usize, len.min(dist) as usize);
    }
    let backend = ScriptedBackend::new(data.clone(), events.to_vec());
    let mut dec = TrackingDecoder::new(backend, TrackerConfig::default().with_window(window).with_min_len(min_len));
    let mut tracker = TarTracker::new(config);
    let mut run = Run { windows: Vec::new(), members: Vec::new(), stats: Default::default(), error: None };
    loop {
        let w = match dec.next_window().unwrap() {
            Some(w) => w,
            None => break,
        };
        match tracker.process(&w) {
            Ok(tw) => {
                run.members.extend(tw.new_members.iter().cloned());
                run.windows.push((tw.copies.to_vec(), tw.spans.to_vec()));
                run.stats = tw.stats;
            }
            Err(e) => {
                run.error = Some(e);
                break;
            }
        }
        if w.is_last {
            break;
        }
    }
    run
}

fn content_offsets(tar: &[u8], members: &[(&str, &[u8])]) -> Vec<(u64, u64)> {
    // Locate each member's content by searching for it (contents are unique random data).
    members
        .iter()
        .map(|(_, d)| {
            let pos = tar.windows(d.len()).position(|w| w == *d).unwrap() as u64;
            (pos, pos + d.len() as u64)
        })
        .collect()
}

#[test]
fn cross_and_intra_member_copies_are_attributed_with_member_offsets() {
    let a = random_bytes(1, 3000);
    let b = random_bytes(2, 3000);
    let members = [("a.fa", a.as_slice()), ("b.fa", b.as_slice())];
    let tar = tar_bytes(&members);
    let offs = content_offsets(&tar, &members);
    // copy 200 bytes from a[100..300] into b[500..700]; and intra-a copy a[1000..1100] -> a[2000..2100]
    let cross = (offs[1].0 + 500, offs[1].0 + 500 - (offs[0].0 + 100), 200);
    let intra = (offs[0].0 + 2000, 1000, 100);
    let r = run(&tar, &[intra, cross], 8192, 10, TarConfig { min_len: 10, ..TarConfig::default() });
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members.len(), 2);
    assert_eq!(r.members[0].name, b"a.fa");
    assert_eq!((r.members[0].content_start, r.members[0].content_end), offs[0]);
    let copies: Vec<_> = r.windows.iter().flat_map(|(c, _)| c.iter().copied()).collect();
    assert!(copies.contains(&MemberCopy::new(0, 1000, 0, 2000, 100)), "{copies:?}");
    assert!(copies.contains(&MemberCopy::new(0, 100, 1, 500, 200)), "{copies:?}");
    assert_eq!(r.stats.cross_member.get(), 1);
    assert_eq!(r.stats.intra_member.get(), 1);
}

#[test]
fn copy_spanning_member_boundary_is_split_and_header_pieces_dropped() {
    let a = random_bytes(3, 2000);
    let b = random_bytes(4, 2000);
    let c = random_bytes(5, 2000);
    let members = [("a", a.as_slice()), ("b", b.as_slice()), ("c", c.as_slice())];
    let tar = tar_bytes(&members);
    let offs = content_offsets(&tar, &members);
    // Source: last 300 bytes of a + a's padding + b's header + first 300 bytes of b.
    let src = offs[0].1 - 300;
    let src_end = offs[1].0 + 300;
    let len = src_end - src;
    // Destination entirely inside c.
    let dst = offs[2].0 + 100;
    let event = (dst, dst - src, len);
    let r = run(&tar, &[event], 8192, 10, TarConfig { min_len: 10, ..TarConfig::default() });
    assert!(r.error.is_none(), "{:?}", r.error);
    let copies: Vec<_> = r.windows.iter().flat_map(|(c, _)| c.iter().copied()).collect();
    assert_eq!(
        copies,
        vec![
            MemberCopy::new(0, 1700, 2, 100, 300),
            MemberCopy::new(1, 0, 2, 100 + 300 + (offs[1].0 - offs[0].1), 300),
        ]
    );
    assert_eq!(r.stats.dropped_header_touch.get(), 1);
    assert_eq!(r.stats.cross_member.get(), 2);
}

#[test]
fn pieces_below_threshold_after_split_are_dropped() {
    let a = random_bytes(6, 2000);
    let b = random_bytes(7, 2000);
    let members = [("a", a.as_slice()), ("b", b.as_slice())];
    let tar = tar_bytes(&members);
    let offs = content_offsets(&tar, &members);
    // 20 bytes of a's tail + gap + 500 bytes of b -> into b later.
    let src = offs[0].1 - 20;
    let len = (offs[1].0 + 500) - src;
    let dst = offs[1].0 + 600;
    let r = run(&tar, &[(dst, dst - src, len)], 8192, 62, TarConfig { min_len: 62, ..TarConfig::default() });
    assert!(r.error.is_none(), "{:?}", r.error);
    let copies: Vec<_> = r.windows.iter().flat_map(|(c, _)| c.iter().copied()).collect();
    let gap = offs[1].0 - offs[0].1;
    assert_eq!(copies, vec![MemberCopy::new(1, 0, 1, 600 + 20 + gap, 500)]);
    assert_eq!(r.stats.dropped_short_piece.get(), 1);
    assert_eq!(r.stats.dropped_header_touch.get(), 1);
}

#[test]
fn gnu_long_name_entry_yields_full_name_and_no_extra_member() {
    let long = "d/".repeat(60) + "genome.fa";
    let a = random_bytes(8, 700);
    let tar = tar_bytes(&[(long.as_str(), a.as_slice()), ("short.fa", a.as_slice())]);
    let r = run(&tar, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members.len(), 2);
    assert_eq!(r.members[0].name, long.as_bytes());
    assert_eq!(r.members[0].header_start, 0, "header_start points at the 'L' entry");
    assert_eq!(r.members[0].len(), 700);
    assert_eq!(r.members[1].name, b"short.fa");
}

#[test]
fn pax_header_overrides_path_and_size() {
    let data = random_bytes(9, 1500);
    let long = "p/".repeat(80) + "seq.fa";
    let mut tar = tar_member_pax(&long, &data);
    tar.extend_from_slice(&tar_end());
    let r = run(&tar, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members.len(), 1);
    assert_eq!(r.members[0].name, long.as_bytes());
    assert_eq!(r.members[0].len(), 1500);
    assert_eq!(r.members[0].header_start, 0);
    assert_eq!(r.members[0].content_start, 1024 + 512);
}

#[test]
fn base256_size_field_is_decoded() {
    let data = random_bytes(10, 600);
    let mut tar = tar_member_base256("b.fa", &data);
    tar.extend_from_slice(&tar_end());
    let r = run(&tar, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members[0].len(), 600);
}

#[test]
fn nested_compressed_members_and_non_files_are_opaque() {
    let data = random_bytes(11, 1000);
    let mut tar = tar_bytes(&[("x.fa.gz", data.as_slice()), ("plain.fa", data.as_slice())]);
    tar.truncate(tar.len() - 1024);
    tar.extend_from_slice(&tar_member_raw("dir/", b'5', Some(0), &[]));
    tar.extend_from_slice(&tar_member_raw("link", b'2', Some(0), &[]));
    tar.extend_from_slice(&tar_end());
    let offs = content_offsets(&tar, &[("x", data.as_slice())]);
    // A copy from the opaque member into plain.fa must be dropped as opaque.
    let dst = offs[0].1 + 512 + 10; // plain.fa content start + 10 (header is 512 bytes, data is a multiple of 512? no: 1000 -> pad 24)
    let _ = dst;
    let r = run(&tar, &[], 8192, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members.len(), 4);
    assert_eq!(r.members[0].opaque, Some(OpaqueReason::CompressedName));
    assert_eq!(r.members[1].opaque, None);
    assert_eq!(r.members[2].opaque, Some(OpaqueReason::NotRegularFile));
    assert_eq!(r.members[3].opaque, Some(OpaqueReason::NotRegularFile));
    assert_eq!(r.stats.opaque_members.get(), 3);

    // Now with a copy from the opaque member into the plain one.
    let plain_start = r.members[1].content_start;
    let opaque_start = r.members[0].content_start;
    let event = (plain_start + 10, plain_start + 10 - opaque_start, 200);
    let r = run(&tar, &[event], 8192, 62, TarConfig::default());
    let copies: Vec<_> = r.windows.iter().flat_map(|(c, _)| c.iter().copied()).collect();
    assert!(copies.is_empty(), "{copies:?}");
    assert_eq!(r.stats.dropped_opaque.get(), 1);
}

#[test]
fn header_straddling_window_boundary_is_parsed() {
    // First member content ends 100 bytes before the window end, so the next header
    // spans the boundary.
    let first_len = W - 512 - 100;
    let a = random_bytes(12, first_len);
    let b = random_bytes(13, 3000);
    let tar = tar_bytes(&[("a", a.as_slice()), ("b", b.as_slice())]);
    let r = run(&tar, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members.len(), 2);
    assert_eq!(r.members[1].name, b"b");
    assert_eq!(r.members[1].len(), 3000);
}

#[test]
fn member_spans_cover_exactly_the_content_bytes_of_each_window() {
    let members: Vec<Vec<u8>> = (0..12).map(|i| random_bytes(100 + i, 300 + 137 * i as usize)).collect();
    let refs: Vec<(&str, &[u8])> = members.iter().map(|m| ("m", m.as_slice())).collect();
    let tar = tar_bytes(&refs);
    let r = run(&tar, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    let total_content: usize = members.iter().map(|m| m.len()).sum();
    let spanned: usize = r.windows.iter().flat_map(|(_, s)| s.iter()).map(|s| s.len).sum();
    assert_eq!(spanned, total_content);
    // Every span maps to the member's own bytes.
    let mut win_start = 0usize;
    for (_, spans) in &r.windows {
        for s in spans {
            let m = &r.members[s.member as usize];
            let abs = win_start + s.window_off;
            assert_eq!(&tar[abs..abs + s.len], &members[s.member as usize][s.member_off as usize..s.member_off as usize + s.len]);
            assert!(abs as u64 >= m.content_start && (abs + s.len) as u64 <= m.content_end);
        }
        win_start += W;
    }
}

#[test]
fn member_indices_are_stable_after_retirement() {
    let members: Vec<Vec<u8>> = (0..200).map(|i| random_bytes(500 + i, 100)).collect();
    let refs: Vec<(&str, &[u8])> = members.iter().map(|m| ("m", m.as_slice())).collect();
    let tar = tar_bytes(&refs);
    // A copy near the end of the archive, between the last two members: every member
    // occupies 512 header bytes plus 512 padded content bytes.
    let dst = 199 * 1024 + 512;
    let event = (dst, 1024, 100);
    let r = run(&tar, &[event], W, 10, TarConfig { min_len: 10, ..TarConfig::default() });
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.members.len(), 200);
    assert_eq!(r.members[199].index, 199);
    let copies: Vec<_> = r.windows.iter().flat_map(|(c, _)| c.iter().copied()).collect();
    assert_eq!(copies, vec![MemberCopy::new(198, 0, 199, 0, 100)]);
}

#[test]
fn truncation_and_corruption_are_errors_and_missing_end_blocks_are_tolerated() {
    let a = random_bytes(14, 3000);
    let tar = tar_bytes(&[("a", a.as_slice())]);

    let mid_header = &tar[..200];
    let r = run(mid_header, &[], W, 62, TarConfig::default());
    assert!(matches!(r.error, Some(TarError::TruncatedHeader { .. })), "{:?}", r.error);

    let mid_content = &tar[..512 + 1000];
    let r = run(mid_content, &[], W, 62, TarConfig::default());
    assert!(matches!(r.error, Some(TarError::TruncatedContent { member: 0, missing: 2000 })), "{:?}", r.error);

    let no_end = &tar[..tar.len() - 1024];
    let r = run(no_end, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none(), "{:?}", r.error);
    let r = run(no_end, &[], W, 62, TarConfig { strict_end: true, ..TarConfig::default() });
    assert_eq!(r.error, Some(TarError::MissingEndBlocks));

    let mut bad = tar.clone();
    bad[10] ^= 0x55;
    let r = run(&bad, &[], W, 62, TarConfig::default());
    assert!(matches!(r.error, Some(TarError::BadChecksum { abs: 0 })), "{:?}", r.error);

    let mut padded = tar.clone();
    padded.extend_from_slice(&vec![0u8; 8192]);
    let r = run(&padded, &[], W, 62, TarConfig::default());
    assert!(r.error.is_none());
    assert_eq!(r.stats.trailing_bytes.get(), 8192);
}

#[test]
fn out_of_order_windows_are_rejected() {
    let a = random_bytes(15, 3000);
    let tar = tar_bytes(&[("a", a.as_slice())]);
    let mut tracker = TarTracker::new(TarConfig::default());
    let w = Window { index: 1, start_abs: 1024, data: &tar[1024..2048], lookahead: &[], copies: &[], back_copies: &[], direction: lz_copyback::Direction::Forward, is_last: false };
    assert!(matches!(tracker.process(&w), Err(TarError::OutOfOrder { expected: 0, got: 1024 })));
}

#[test]
fn sparse_extended_entries_are_unsupported() {
    let mut header = tar::Header::new_gnu();
    header.set_path("sparse").unwrap();
    header.set_size(0);
    header.as_mut_bytes()[156] = b'S';
    header.as_mut_bytes()[482] = 1; // isextended
    header.set_cksum();
    let mut tar = header.as_bytes().to_vec();
    tar.extend_from_slice(&tar_end());
    let r = run(&tar, &[], W, 62, TarConfig::default());
    assert!(matches!(r.error, Some(TarError::UnsupportedEntry { abs: 0, typeflag: b'S' })), "{:?}", r.error);
    let _ = tar_block(&[]);
}

#[test]
fn real_xz_tar_reports_cross_member_copies_that_match_member_bytes() {
    // Ten genomes: five distinct, five copies of earlier ones with a few edits.
    let mut genomes: Vec<Vec<u8>> = (0..5).map(|i| fasta(700 + i, 8, 4096, 0.0, 1)).collect();
    for i in 0..5 {
        let mut g = genomes[i].clone();
        for k in (100..g.len()).step_by(7000) {
            g[k] = b'N';
        }
        genomes.push(g);
    }
    let names: Vec<String> = (0..10).map(|i| format!("batch/SAMN{i:07}.fa")).collect();
    let members: Vec<(&str, &[u8])> = names.iter().zip(&genomes).map(|(n, g)| (n.as_str(), g.as_slice())).collect();
    let tar = tar_bytes(&members);
    let path = temp_path("genomes.tar.xz");
    std::fs::write(&path, xz_encode(&tar, 1 << 20)).unwrap();

    let mut dec = lz_copyback::open(&path, TrackerConfig::default()).unwrap();
    let mut tracker = TarTracker::new(TarConfig::default());
    let mut member_copies = Vec::new();
    let mut members = Vec::new();
    loop {
        let w = dec.next_window().unwrap().expect("windows until is_last");
        let tw = tracker.process(&w).unwrap();
        members.extend(tw.new_members.iter().cloned());
        member_copies.extend_from_slice(tw.copies);
        if w.is_last {
            break;
        }
    }
    let _ = std::fs::remove_file(&path);
    assert_eq!(members.len(), 10);
    let last_stats = tracker.stats();
    assert!(last_stats.cross_member.get() > 0, "{last_stats:?}");
    assert!(!member_copies.is_empty());
    for c in &member_copies {
        let s = &genomes[c.src_member as usize][c.src_off() as usize..(c.src_off() + c.len()) as usize];
        let d = &genomes[c.dst_member as usize][c.dst_off() as usize..(c.dst_off() + c.len()) as usize];
        assert_eq!(s, d, "{c:?}");
        assert!(c.len() >= 62);
    }
    // Some copy covers a large part of a duplicated genome.
    assert!(member_copies.iter().any(|c| c.src_member < 5 && c.dst_member >= 5 && c.len() > 4000), "{:?}", &member_copies[..member_copies.len().min(10)]);
    drain(&mut TrackingDecoder::new(ScriptedBackend::new(vec![], vec![]), TrackerConfig::default()), &[], 62);
    let _ = LzCopy::new(0, 1, 1);
}
