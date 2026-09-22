//! Incremental parser of tar headers over an absolute byte stream.

use super::{MemberInfo, OpaqueReason, TarError};
use tar::{EntryType, Header};

const BLOCK: u64 = 512;
/// Cap on GNU long-name / PAX payloads, to bound memory on corrupt input.
const MAX_EXT: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExtKind {
    LongName,
    LongLink,
    Pax,
    PaxGlobal,
}

#[derive(Debug)]
enum State {
    /// Accumulating a 512-byte header block (or end-of-archive zero block).
    Header,
    Content { member: u32, remaining: u64 },
    Padding { remaining: u64 },
    Ext { kind: ExtKind, remaining: u64, buf: Vec<u8>, start: u64 },
    /// One all-zero block seen; a second one ends the archive.
    ZeroBlock1,
    End,
}

#[derive(Default)]
struct PaxFields {
    path: Option<Vec<u8>>,
    size: Option<u64>,
}

pub(crate) struct HeaderParser {
    state: State,
    partial: [u8; BLOCK as usize],
    partial_len: usize,
    /// Absolute offset of the next byte to parse.
    cursor: u64,
    pending_long_name: Option<Vec<u8>>,
    pending_pax: PaxFields,
    /// Start of the first extension header belonging to the member being assembled.
    ext_start: Option<u64>,
    next_index: u32,
    trailing: u64,
}

fn all_zero(block: &[u8]) -> bool {
    block.iter().all(|&b| b == 0)
}

/// Both checksum conventions GNU tar accepts: unsigned bytes and signed bytes.
fn checksums(block: &[u8]) -> (u32, i64) {
    let mut unsigned = 0u32;
    let mut signed = 0i64;
    for (i, &b) in block.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += b as u32;
        signed += b as i8 as i64;
    }
    (unsigned, signed)
}

fn pad_after(end: u64) -> u64 {
    (BLOCK - end % BLOCK) % BLOCK
}

impl HeaderParser {
    pub(crate) fn new() -> Self {
        Self {
            state: State::Header,
            partial: [0; BLOCK as usize],
            partial_len: 0,
            cursor: 0,
            pending_long_name: None,
            pending_pax: PaxFields::default(),
            ext_start: None,
            next_index: 0,
            trailing: 0,
        }
    }

    pub(crate) fn trailing_bytes(&self) -> u64 {
        self.trailing
    }

    /// Feed the bytes at absolute offset `abs`. Bytes before the cursor are skipped, a gap
    /// after it is an error.
    pub(crate) fn feed(
        &mut self,
        abs: u64,
        bytes: &[u8],
        opaque: &dyn Fn(&[u8]) -> bool,
        out: &mut Vec<MemberInfo>,
    ) -> Result<(), TarError> {
        let end = abs + bytes.len() as u64;
        if end <= self.cursor {
            return Ok(());
        }
        if abs > self.cursor {
            return Err(TarError::OutOfOrder { expected: self.cursor, got: abs });
        }
        let mut bytes = &bytes[(self.cursor - abs) as usize..];
        while !bytes.is_empty() {
            let avail = bytes.len() as u64;
            match &mut self.state {
                State::Header | State::ZeroBlock1 => {
                    let take = (BLOCK as usize - self.partial_len).min(bytes.len());
                    self.partial[self.partial_len..self.partial_len + take].copy_from_slice(&bytes[..take]);
                    self.partial_len += take;
                    self.cursor += take as u64;
                    bytes = &bytes[take..];
                    if self.partial_len == BLOCK as usize {
                        self.partial_len = 0;
                        self.parse_block(opaque, out)?;
                    }
                }
                State::Content { member, remaining } => {
                    let take = (*remaining).min(avail);
                    *remaining -= take;
                    let _ = member;
                    self.cursor += take;
                    bytes = &bytes[take as usize..];
                    if *remaining == 0 {
                        let pad = pad_after(self.cursor);
                        self.state = if pad == 0 { State::Header } else { State::Padding { remaining: pad } };
                    }
                }
                State::Padding { remaining } => {
                    let take = (*remaining).min(avail);
                    *remaining -= take;
                    self.cursor += take;
                    bytes = &bytes[take as usize..];
                    if *remaining == 0 {
                        self.state = State::Header;
                    }
                }
                State::Ext { kind, remaining, buf, start } => {
                    let take = (*remaining).min(avail);
                    buf.extend_from_slice(&bytes[..take as usize]);
                    *remaining -= take;
                    self.cursor += take;
                    bytes = &bytes[take as usize..];
                    if *remaining == 0 {
                        let (kind, buf, start) = (*kind, std::mem::take(buf), *start);
                        self.apply_ext(kind, buf, start)?;
                        let pad = pad_after(self.cursor);
                        self.state = if pad == 0 { State::Header } else { State::Padding { remaining: pad } };
                    }
                }
                State::End => {
                    self.trailing += avail;
                    self.cursor += avail;
                    bytes = &[];
                }
            }
        }
        Ok(())
    }

    fn parse_block(&mut self, opaque: &dyn Fn(&[u8]) -> bool, out: &mut Vec<MemberInfo>) -> Result<(), TarError> {
        let block_start = self.cursor - BLOCK;
        if all_zero(&self.partial) {
            self.state = match self.state {
                State::ZeroBlock1 => State::End,
                _ => State::ZeroBlock1,
            };
            return Ok(());
        }
        // A non-zero block after a single zero block: GNU tar keeps reading.
        let header = Header::from_byte_slice(&self.partial);
        let stored = header.cksum().map_err(|_| TarError::BadChecksum { abs: block_start })?;
        let (unsigned, signed) = checksums(&self.partial);
        if stored != unsigned && stored as i64 != signed {
            return Err(TarError::BadChecksum { abs: block_start });
        }
        let typeflag = self.partial[156];
        let entry_type = header.entry_type();
        let entry_size = header
            .entry_size()
            .map_err(|_| TarError::BadField { abs: block_start, what: "size" })?;

        let ext = if entry_type.is_gnu_longname() {
            Some(ExtKind::LongName)
        } else if entry_type == EntryType::GNULongLink {
            Some(ExtKind::LongLink)
        } else if entry_type.is_pax_local_extensions() {
            Some(ExtKind::Pax)
        } else if entry_type.is_pax_global_extensions() {
            Some(ExtKind::PaxGlobal)
        } else {
            None
        };
        if let Some(kind) = ext {
            if entry_size > MAX_EXT {
                return Err(TarError::BadPax { abs: block_start });
            }
            if kind != ExtKind::PaxGlobal {
                self.ext_start.get_or_insert(block_start);
            }
            self.state = if entry_size == 0 {
                self.apply_ext(kind, Vec::new(), block_start)?;
                State::Header
            } else {
                State::Ext { kind, remaining: entry_size, buf: Vec::with_capacity(entry_size as usize), start: block_start }
            };
            return Ok(());
        }

        if entry_type.is_gnu_sparse() && header.as_gnu().is_some_and(|g| g.is_extended()) {
            return Err(TarError::UnsupportedEntry { abs: block_start, typeflag });
        }

        let pax = std::mem::take(&mut self.pending_pax);
        let size = pax.size.unwrap_or(entry_size);
        let name = pax
            .path
            .or_else(|| self.pending_long_name.take())
            .unwrap_or_else(|| header.path_bytes().into_owned());
        let opaque_reason = if !entry_type.is_file() {
            Some(OpaqueReason::NotRegularFile)
        } else if opaque(&name) {
            Some(OpaqueReason::CompressedName)
        } else {
            None
        };
        let index = self.next_index;
        self.next_index += 1;
        out.push(MemberInfo {
            index,
            name,
            header_start: self.ext_start.take().unwrap_or(block_start),
            content_start: self.cursor,
            content_end: self.cursor + size,
            opaque: opaque_reason,
        });
        self.state = if size == 0 { State::Header } else { State::Content { member: index, remaining: size } };
        Ok(())
    }

    fn apply_ext(&mut self, kind: ExtKind, buf: Vec<u8>, start: u64) -> Result<(), TarError> {
        match kind {
            ExtKind::LongName => {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                self.pending_long_name = Some(buf[..end].to_vec());
            }
            ExtKind::LongLink | ExtKind::PaxGlobal => {}
            ExtKind::Pax => {
                for ext in tar::PaxExtensions::new(&buf) {
                    let ext = ext.map_err(|_| TarError::BadPax { abs: start })?;
                    match ext.key_bytes() {
                        b"path" => self.pending_pax.path = Some(ext.value_bytes().to_vec()),
                        b"size" => {
                            let v = std::str::from_utf8(ext.value_bytes())
                                .ok()
                                .and_then(|s| s.trim().parse::<u64>().ok())
                                .ok_or(TarError::BadPax { abs: start })?;
                            self.pending_pax.size = Some(v);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(&self, strict_end: bool) -> Result<(), TarError> {
        match &self.state {
            State::Header if self.partial_len > 0 => {
                Err(TarError::TruncatedHeader { abs: self.cursor - self.partial_len as u64 })
            }
            State::Header => {
                if strict_end { Err(TarError::MissingEndBlocks) } else { Ok(()) }
            }
            State::ZeroBlock1 if self.partial_len > 0 => {
                Err(TarError::TruncatedHeader { abs: self.cursor - self.partial_len as u64 })
            }
            State::ZeroBlock1 | State::End => Ok(()),
            State::Content { member, remaining } => {
                Err(TarError::TruncatedContent { member: *member, missing: *remaining })
            }
            State::Padding { remaining } => Err(TarError::TruncatedContent {
                member: self.next_index.saturating_sub(1),
                missing: *remaining,
            }),
            State::Ext { start, .. } => Err(TarError::TruncatedHeader { abs: *start }),
        }
    }
}
