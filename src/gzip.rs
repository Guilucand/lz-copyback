//! gzip decoding over `streaming-libdeflate-rs`, with the interface of its
//! `LibdeflateGzipFileDecompressor` and `LibdeflateGzipPushDecompressor`, plus a [`Read`]
//! adapter. The matches come from its `on_lz_copy` hook.

use crate::copybacks::{copyback_methods, Copybacks};
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::Path;
use streaming_libdeflate_rs::decompress_deflate::{OutStreamResult, MAX_WRITE};
use streaming_libdeflate_rs::decompress_gzip::LibdeflateGzipDecompressor;
use streaming_libdeflate_rs::streams::deflate_chunked_buffer_input::DeflateChunkedBufferInput;
use streaming_libdeflate_rs::streams::deflate_chunked_buffer_output::DeflateChunkedBufferOutput;
use streaming_libdeflate_rs::{libdeflate_alloc_decode_tables, DeflateInput, DeflateOutput, LibdeflateDecodeTables};

pub use streaming_libdeflate_rs::{LibdeflateDecompressResult, LibdeflateError};

/// A stream of concatenated gzip members, shared by the file and push decompressors (the same
/// loop as `streaming-libdeflate-rs`'s own).
struct GzipStream {
    input: DeflateChunkedBufferInput<'static>,
    output: TrackingOutput,
    tables: LibdeflateDecodeTables,
    gzip: LibdeflateGzipDecompressor,
    finished: bool,
}

impl GzipStream {
    fn new(input: DeflateChunkedBufferInput<'static>, buf_size: usize) -> Self {
        Self {
            input,
            // The deflate loop only decodes while MAX_WRITE bytes are writable, so a smaller
            // output buffer would never make progress.
            output: TrackingOutput::new(DeflateChunkedBufferOutput::new(buf_size.max(MAX_WRITE))),
            tables: libdeflate_alloc_decode_tables(),
            gzip: LibdeflateGzipDecompressor::new(),
            finished: false,
        }
    }

    fn decompress(&mut self) -> Result<LibdeflateDecompressResult, LibdeflateError> {
        if self.finished {
            return Ok(if self.output.buffer.pending_output().is_empty() {
                LibdeflateDecompressResult::EndOfStream
            } else {
                LibdeflateDecompressResult::MoreData
            });
        }
        if !self.output.buffer.has_writable_length(MAX_WRITE) {
            return Ok(LibdeflateDecompressResult::MoreData);
        }
        loop {
            // While tracking, the decoder writes through the tracking output, whose
            // `on_lz_copy` records the matches; otherwise to the bare buffer, so that its
            // decoding loop is the stock one, with no hook at all.
            let result = if self.output.copybacks.tracking {
                self.output.begin();
                self.gzip.decompress(&mut self.tables, &mut self.input, &mut self.output)
            } else {
                self.gzip.decompress(&mut self.tables, &mut self.input, &mut self.output.buffer)
            };
            match result? {
                LibdeflateDecompressResult::EndOfStream => {
                    if !self.output.buffer.pending_output().is_empty() {
                        return Ok(LibdeflateDecompressResult::MoreData);
                    }
                    // Another member follows if there is any input left.
                    if !self.input.ensure_available(1) {
                        if !self.input.is_eof() {
                            return Ok(LibdeflateDecompressResult::NeedInput);
                        }
                        self.finished = true;
                        return Ok(LibdeflateDecompressResult::EndOfStream);
                    }
                    self.gzip = LibdeflateGzipDecompressor::new();
                }
                result => return Ok(result),
            }
        }
    }

    fn pending_output(&self) -> &[u8] {
        self.output.buffer.pending_output()
    }

    fn consume_output(&mut self, consumed_bytes: usize) {
        self.output.buffer.consume_output(consumed_bytes);
        self.output.consumed += consumed_bytes as u64;
    }

    fn increase_buffer_size(&mut self, additional: usize) {
        self.output.buffer.increase_buffer_size(additional);
    }
}

/// The decompressor's output buffer and what recording its matches needs. It owns the buffer,
/// rather than borrowing it, so that the tracked decoding loop reaches the buffer's fields
/// directly, as the untracked one does.
struct TrackingOutput {
    buffer: DeflateChunkedBufferOutput<'static>,
    copybacks: Copybacks,
    /// Output bytes consumed so far: the absolute offset of the first pending byte.
    consumed: u64,
    /// Address of the first pending byte; set by [`Self::begin`] before every decoding call.
    base: usize,
    /// The threshold, at least 1 so that entries with literals only, which report a zero
    /// length, never pass; set by [`Self::begin`].
    min_len: u32,
}

impl TrackingOutput {
    fn new(buffer: DeflateChunkedBufferOutput<'static>) -> Self {
        Self {
            buffer,
            copybacks: Copybacks::new(),
            consumed: 0,
            base: 0,
            min_len: 1,
        }
    }

    fn begin(&mut self) {
        self.rebase();
        self.min_len = self.copybacks.min_len.max(1);
    }

    fn rebase(&mut self) {
        self.base = self.buffer.pending_output().as_ptr() as usize;
    }
}

impl DeflateOutput for TrackingOutput {
    const MAX_LOOK_BACK: usize = <DeflateChunkedBufferOutput<'static> as DeflateOutput>::MAX_LOOK_BACK;
    const OVERWRITE_MAX: usize = <DeflateChunkedBufferOutput<'static> as DeflateOutput>::OVERWRITE_MAX;
    const TRACK_LZ: bool = true;

    #[inline(always)]
    fn has_writable_length(&mut self, length: usize) -> bool {
        self.buffer.has_writable_length(length)
    }

    #[inline(always)]
    fn get_output_ptr(&mut self) -> *mut u8 {
        self.buffer.get_output_ptr()
    }

    #[inline(always)]
    unsafe fn set_output_ptr(&mut self, ptr: *mut u8) {
        unsafe { self.buffer.set_output_ptr(ptr) }
    }

    fn pending_output(&self) -> &[u8] {
        self.buffer.pending_output()
    }

    fn consume_output(&mut self, consumed_offset: usize) {
        self.buffer.consume_output(consumed_offset);
        self.consumed += consumed_offset as u64;
        self.rebase();
    }

    fn increase_buffer_size(&mut self, additional: usize) {
        self.buffer.increase_buffer_size(additional);
        self.rebase();
    }

    #[inline(always)]
    fn finish_member(&mut self) -> Result<OutStreamResult, ()> {
        self.buffer.finish_member()
    }

    #[inline(always)]
    fn on_lz_copy(&mut self, dst: *const u8, distance: usize, len: usize) {
        if len as u32 >= self.min_len {
            let destination = self.consumed + (dst as usize - self.base) as u64;
            self.copybacks.push(destination, distance as u32, len as u32);
        }
    }
}

/// A gzip file decompressor that also records the LZ77 matches it decodes, with the
/// interface of `LibdeflateGzipFileDecompressor`.
///
/// `decompress` decodes into a buffer of its own and records the matches as it goes, so they
/// are in [`Self::peek_copybacks`] before their bytes are in [`Self::pending_output`].
pub struct GzipFileDecompressor {
    stream: GzipStream,
}

impl GzipFileDecompressor {
    /// Opens `file`; panics if it cannot be opened.
    pub fn new(file: impl AsRef<Path>, buf_size: usize) -> Self {
        Self::try_new(file, buf_size).unwrap()
    }

    pub fn try_new(file: impl AsRef<Path>, buf_size: usize) -> io::Result<Self> {
        let mut read_file = File::open(file)?;
        let input = DeflateChunkedBufferInput::new(
            move |buf| loop {
                match read_file.read(buf) {
                    Ok(count) => return count,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => return 0,
                }
            },
            buf_size,
        );
        Ok(Self {
            stream: GzipStream::new(input, buf_size),
        })
    }

    /// Decodes until the output buffer must be consumed (`MoreData`) or the stream ends.
    pub fn decompress(&mut self) -> Result<LibdeflateDecompressResult, LibdeflateError> {
        self.stream.decompress()
    }

    pub fn pending_output(&self) -> &[u8] {
        self.stream.pending_output()
    }

    pub fn consume_output(&mut self, consumed_bytes: usize) {
        self.stream.consume_output(consumed_bytes);
    }

    pub fn increase_buffer_size(&mut self, additional: usize) {
        self.stream.increase_buffer_size(additional);
    }

    copyback_methods!(stream.output.copybacks);
}

/// A gzip decompressor fed by the caller that also records the LZ77 matches it decodes, with
/// the interface of `LibdeflateGzipPushDecompressor`.
///
/// `decompress` returns `MoreData` when the output buffer must be consumed, `NeedInput` when
/// the pushed data is used up, and `EndOfStream` once the input is finished and fully
/// decoded. The matches are recorded as they are decoded, so they are in
/// [`Self::peek_copybacks`] before their bytes are in [`Self::pending_output`].
pub struct GzipPushDecompressor {
    stream: GzipStream,
}

impl GzipPushDecompressor {
    pub fn new(buf_size: usize) -> Self {
        Self {
            stream: GzipStream::new(DeflateChunkedBufferInput::new_push(buf_size), buf_size),
        }
    }

    /// Copies as much of `data` as fits into the input buffer, returning the bytes taken.
    pub fn push_input(&mut self, data: &[u8]) -> usize {
        self.stream.input.push_input(data)
    }

    /// The free part of the input buffer, to be written and then committed with
    /// [`Self::commit_input`].
    pub fn input_space_mut(&mut self) -> &mut [u8] {
        self.stream.input.input_space_mut()
    }

    pub fn commit_input(&mut self, len: usize) {
        self.stream.input.commit_input(len);
    }

    /// Marks the end of the compressed stream.
    pub fn finish_input(&mut self) {
        self.stream.input.finish_input();
    }

    pub fn decompress(&mut self) -> Result<LibdeflateDecompressResult, LibdeflateError> {
        self.stream.decompress()
    }

    pub fn pending_output(&self) -> &[u8] {
        self.stream.pending_output()
    }

    pub fn consume_output(&mut self, consumed_bytes: usize) {
        self.stream.consume_output(consumed_bytes);
    }

    pub fn increase_buffer_size(&mut self, additional: usize) {
        self.stream.increase_buffer_size(additional);
    }

    /// Decompresses all of `data`, passing full output buffers to `func`. Output that does
    /// not fill a buffer stays pending until a later call or [`Self::finish`].
    pub fn push(
        &mut self,
        mut data: &[u8],
        mut func: impl FnMut(&[u8]) -> Result<(), ()>,
    ) -> Result<(), LibdeflateError> {
        loop {
            let accepted = self.push_input(data);
            data = &data[accepted..];
            match self.decompress()? {
                LibdeflateDecompressResult::MoreData => self.flush_output(&mut func)?,
                LibdeflateDecompressResult::NeedInput => {
                    if data.is_empty() {
                        return Ok(());
                    }
                }
                LibdeflateDecompressResult::EndOfStream => return Ok(()),
            }
        }
    }

    /// Marks the end of the input and passes all the remaining output to `func`.
    pub fn finish(&mut self, mut func: impl FnMut(&[u8]) -> Result<(), ()>) -> Result<(), LibdeflateError> {
        self.finish_input();
        loop {
            match self.decompress()? {
                LibdeflateDecompressResult::MoreData => self.flush_output(&mut func)?,
                LibdeflateDecompressResult::NeedInput => unreachable!("the input is finished"),
                LibdeflateDecompressResult::EndOfStream => return Ok(()),
            }
        }
    }

    fn flush_output(&mut self, func: &mut impl FnMut(&[u8]) -> Result<(), ()>) -> Result<(), LibdeflateError> {
        func(self.pending_output()).map_err(|_| LibdeflateError::InsufficientSpace)?;
        self.consume_output(self.pending_output().len());
        Ok(())
    }

    copyback_methods!(stream.output.copybacks);
}

/// Gzip from any reader as a [`Read`]: the compressed bytes are read straight into the push
/// decompressor's input buffer, and its output is handed out of its own buffer.
pub struct GzipDecoder<R> {
    source: R,
    decompressor: GzipPushDecompressor,
    /// How much of the decompressor's current output buffer `read` already handed out.
    pending_read: usize,
}

impl<R: Read> GzipDecoder<R> {
    /// Decodes `source` with a 1 MiB output buffer.
    pub fn new(source: R) -> Self {
        Self::with_buffer_size(source, 1 << 20)
    }

    pub fn with_buffer_size(source: R, buf_size: usize) -> Self {
        Self {
            source,
            decompressor: GzipPushDecompressor::new(buf_size),
            pending_read: 0,
        }
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.source
    }

    /// The underlying reader. Input already buffered by the decoder is lost.
    pub fn into_inner(self) -> R {
        self.source
    }

    copyback_methods!(decompressor.stream.output.copybacks);

    /// Decodes until there is pending output, returning false at the end of the stream.
    fn next_output(&mut self) -> io::Result<bool> {
        loop {
            let result = self
                .decompressor
                .decompress()
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, format!("gzip: {error:?}")))?;
            match result {
                LibdeflateDecompressResult::EndOfStream => return Ok(false),
                _ if !self.decompressor.pending_output().is_empty() => return Ok(true),
                LibdeflateDecompressResult::NeedInput => self.read_input()?,
                LibdeflateDecompressResult::MoreData => {}
            }
        }
    }

    /// Reads more compressed bytes into the decompressor, or marks the end of the input.
    fn read_input(&mut self) -> io::Result<()> {
        let space = self.decompressor.input_space_mut();
        if space.is_empty() {
            return Err(io::Error::other("gzip: the input buffer is full"));
        }
        let count = loop {
            match self.source.read(space) {
                Ok(count) => break count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        };
        if count == 0 {
            self.decompressor.finish_input();
        } else {
            self.decompressor.commit_input(count);
        }
        Ok(())
    }
}

impl<R: Read> Read for GzipDecoder<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.pending_read == self.decompressor.pending_output().len() {
            // The output buffer is released only once all of it was read: releasing part of
            // it would make the decompressor move the rest back to its front at every read.
            self.decompressor.consume_output(self.pending_read);
            self.pending_read = 0;
            if !self.next_output()? {
                return Ok(0);
            }
        }
        let pending = &self.decompressor.pending_output()[self.pending_read..];
        let count = pending.len().min(buffer.len());
        buffer[..count].copy_from_slice(&pending[..count]);
        self.pending_read += count;
        Ok(count)
    }
}
