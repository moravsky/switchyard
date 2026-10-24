use crate::window::{Capture, Window};
use crate::{bad, Model, Result};
use std::io::Read;

fn invalid() -> crate::Fault {
    bad("request body is not valid JSON")
}

pub fn scan<R: Read>(reader: R, cap: usize) -> Result<Model> {
    let mut parser = Parser {
        input: Window::new(reader),
        stack: Vec::new(),
        model: Model::default(),
        cap,
        pending: false,
        count: 0,
    };
    let first = parser.whitespace()?;
    parser.value(first)?;
    while let Some(c) = parser.input.next()? {
        if !matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            return Err(invalid());
        }
    }
    Ok(parser.model)
}

struct Parser<R> {
    input: Window<R>,
    stack: Vec<u8>,
    model: Model,
    cap: usize,
    pending: bool,
    count: usize,
}

impl<R: Read> Parser<R> {
    fn byte(&mut self) -> Result<u8> {
        self.input.next()?.ok_or_else(invalid)
    }
    fn whitespace(&mut self) -> Result<u8> {
        loop {
            let c = self.byte()?;
            if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                return Ok(c);
            }
        }
    }
    fn closer(c: u8) -> u8 {
        if c == b'{' {
            b'}'
        } else {
            b']'
        }
    }

    fn value(&mut self, mut c: u8) -> Result<()> {
        loop {
            let is_model = std::mem::take(&mut self.pending);
            let mut opened = false;
            match c {
                b'{' | b'[' => {
                    if self.stack.len() >= 10_000 {
                        return Err(invalid());
                    }
                    self.stack.push(c);
                    c = self.whitespace()?;
                    let top = *self.stack.last().unwrap();
                    if c == Self::closer(top) {
                        self.stack.pop();
                    } else {
                        if top == b'{' {
                            self.member(c)?;
                            c = self.whitespace()?;
                        }
                        opened = true;
                    }
                }
                b'"' => {
                    let start = self.input.offset() - 1;
                    let capture = self.string(if is_model { self.cap } else { 0 })?;
                    if is_model {
                        self.model.present = true;
                        self.model.value = capture.bytes;
                        self.model.over = capture.over;
                        self.model.range = Some((start, self.input.offset()));
                    }
                }
                b't' => self.literal(b"rue")?,
                b'f' => self.literal(b"alse")?,
                b'n' => self.literal(b"ull")?,
                b'-' | b'0'..=b'9' => self.number(c)?,
                _ => return Err(invalid()),
            }
            if opened {
                continue;
            }
            loop {
                let Some(&top) = self.stack.last() else {
                    return Ok(());
                };
                c = self.whitespace()?;
                if c == b',' {
                    c = self.whitespace()?;
                    if top == b'{' {
                        self.member(c)?;
                        c = self.whitespace()?;
                    }
                    break;
                }
                if c != Self::closer(top) {
                    return Err(invalid());
                }
                self.stack.pop();
            }
        }
    }

    fn member(&mut self, c: u8) -> Result<()> {
        if c != b'"' {
            return Err(invalid());
        }
        let key = self.string(if self.stack.len() == 1 { 5 } else { 0 })?;
        if self.stack.len() == 1 && !key.over && key.bytes == b"model" {
            self.count += 1;
            if self.count > 1 {
                return Err(bad("duplicate top-level model field"));
            }
            self.pending = true;
        }
        if self.whitespace()? != b':' {
            return Err(invalid());
        }
        Ok(())
    }

    fn string(&mut self, cap: usize) -> Result<Capture> {
        let mut capture = Capture::new(cap);
        loop {
            if self.input.ensure(1)? == 0 {
                return Err(invalid());
            }
            let chunk = &self.input.buf[self.input.pos..self.input.end];
            let n = chunk
                .iter()
                .position(|&c| c == b'"' || c == b'\\' || !(0x20..0x80).contains(&c))
                .unwrap_or(chunk.len());
            capture.push(&chunk[..n]);
            self.input.pos += n;
            if self.input.pos == self.input.end {
                continue;
            }
            let c = self.byte()?;
            match c {
                b'"' => return Ok(capture),
                b'\\' => self.escape(&mut capture)?,
                0..=0x1f => return Err(invalid()),
                _ => {
                    let n = match c {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(invalid()),
                    };
                    let mut bytes = [0; 4];
                    bytes[0] = c;
                    for byte in &mut bytes[1..n] {
                        *byte = self.byte()?;
                    }
                    std::str::from_utf8(&bytes[..n]).map_err(|_| invalid())?;
                    capture.push(&bytes[..n]);
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let mut value = 0;
        for _ in 0..4 {
            value = (value << 4) | (self.byte()? as char).to_digit(16).ok_or_else(invalid)?;
        }
        Ok(value)
    }

    fn escape(&mut self, capture: &mut Capture) -> Result<()> {
        let c = self.byte()?;
        let byte = match c {
            b'"' | b'\\' | b'/' => c,
            b'b' => 8,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let mut value = self.hex4()?;
                if (0xd800..0xdc00).contains(&value) && self.input.ensure(6)? >= 6 {
                    let start = self.input.pos;
                    if &self.input.buf[start..start + 2] == b"\\u" {
                        let tail = &self.input.buf[start + 2..start + 6];
                        let low = tail
                            .iter()
                            .try_fold(0u32, |n, &c| (c as char).to_digit(16).map(|v| (n << 4) | v));
                        if let Some(low) = low.filter(|v| (0xdc00..0xe000).contains(v)) {
                            self.input.pos += 6;
                            value = 0x10000 + ((value - 0xd800) << 10) + low - 0xdc00;
                        }
                    }
                }
                let mut bytes = [0; 4];
                capture.push(
                    char::from_u32(value)
                        .unwrap_or('\u{fffd}')
                        .encode_utf8(&mut bytes)
                        .as_bytes(),
                );
                return Ok(());
            }
            _ => return Err(invalid()),
        };
        capture.push(&[byte]);
        Ok(())
    }

    fn literal(&mut self, remaining: &[u8]) -> Result<()> {
        for &c in remaining {
            if self.byte()? != c {
                return Err(invalid());
            }
        }
        Ok(())
    }
    fn digits(&mut self) -> Result<usize> {
        let mut count = 0;
        while self.input.peek()?.is_some_and(|c| c.is_ascii_digit()) {
            self.input.pos += 1;
            count += 1;
        }
        Ok(count)
    }
    fn number(&mut self, mut c: u8) -> Result<()> {
        if c == b'-' {
            c = self.byte()?;
        }
        match c {
            b'0' => {}
            b'1'..=b'9' => {
                self.digits()?;
            }
            _ => return Err(invalid()),
        }
        if self.input.peek()? == Some(b'.') {
            self.input.pos += 1;
            if self.digits()? == 0 {
                return Err(invalid());
            }
        }
        if matches!(self.input.peek()?, Some(b'e' | b'E')) {
            self.input.pos += 1;
            if matches!(self.input.peek()?, Some(b'+' | b'-')) {
                self.input.pos += 1;
            }
            if self.digits()? == 0 {
                return Err(invalid());
            }
        }
        Ok(())
    }
}
