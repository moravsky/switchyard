use crate::{bad, Fault, Result, BUFFER};
use std::io::Read;

pub struct Window<R> {
    reader: R,
    pub buf: Vec<u8>,
    pub pos: usize,
    pub end: usize,
    base: u64,
    eof: bool,
}

impl<R: Read> Window<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: vec![0; BUFFER],
            pos: 0,
            end: 0,
            base: 0,
            eof: false,
        }
    }

    pub fn offset(&self) -> u64 {
        self.base + self.pos as u64
    }

    pub fn ensure(&mut self, n: usize) -> Result<usize> {
        if n > self.buf.len() {
            return Err(bad("input token is too long"));
        }
        if self.end - self.pos >= n || self.eof {
            return Ok(self.end - self.pos);
        }
        self.buf.copy_within(self.pos..self.end, 0);
        self.base += self.pos as u64;
        self.end -= self.pos;
        self.pos = 0;
        while self.end < n && !self.eof {
            let got = self
                .reader
                .read(&mut self.buf[self.end..])
                .map_err(Fault::io)?;
            if got == 0 {
                self.eof = true;
            }
            self.end += got;
        }
        Ok(self.end - self.pos)
    }

    pub fn next(&mut self) -> Result<Option<u8>> {
        if self.ensure(1)? == 0 {
            return Ok(None);
        }
        let c = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(c))
    }

    pub fn peek(&mut self) -> Result<Option<u8>> {
        Ok(if self.ensure(1)? == 0 {
            None
        } else {
            Some(self.buf[self.pos])
        })
    }

    pub fn find(&mut self, pattern: &[u8]) -> Result<bool> {
        loop {
            if let Some(i) = memchr::memmem::find(&self.buf[self.pos..self.end], pattern) {
                self.pos += i;
                return Ok(true);
            }
            let keep = pattern.len() - 1;
            if self.end - self.pos > keep {
                self.pos = self.end - keep;
            }
            let available = self.end - self.pos;
            if self.ensure(available + 1)? <= available {
                return Ok(false);
            }
        }
    }

    pub fn line(&mut self, budget: &mut usize) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let c = self.next()?.ok_or_else(|| bad("incomplete header line"))?;
            *budget = budget
                .checked_sub(1)
                .ok_or_else(|| bad("header is too large"))?;
            if c == b'\n' {
                if line.pop() != Some(b'\r') || line.contains(&b'\r') {
                    return Err(bad("invalid line ending"));
                }
                return Ok(line);
            }
            line.push(c);
        }
    }
}

pub struct Capture {
    pub bytes: Vec<u8>,
    limit: usize,
    pub over: bool,
}

impl Capture {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            over: false,
        }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(self.limit - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..take]);
        self.over |= take < bytes.len();
    }
}
