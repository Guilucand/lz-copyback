use crate::backend::{BackendError, LzBackend};
use crate::recorder::CopyRecorder;
use std::io::{ErrorKind, Read};

/// A backend for formats without an LZ stage (plain files, bzip2): it copies bytes
/// through and never reports a copy.
pub struct Passthrough<R: Read> {
    reader: R,
}

impl<R: Read> Passthrough<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: Read> LzBackend for Passthrough<R> {
    fn max_distance(&self) -> u64 {
        0
    }

    fn decode(
        &mut self,
        buf: &mut [u8],
        pos: usize,
        target: usize,
        _rec: &mut CopyRecorder,
    ) -> Result<usize, BackendError> {
        loop {
            match self.reader.read(&mut buf[pos..target]) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(Box::new(e)),
            }
        }
    }
}
