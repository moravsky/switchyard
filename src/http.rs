use crate::{bad, Fault, Result, BUFFER};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<Header>,
}
pub struct Response {
    pub line: Vec<u8>,
    pub status: u16,
    pub headers: Vec<Header>,
}
pub struct Spool {
    pub file: File,
    pub size: u64,
}

pub fn token(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&c| tchar(c))
}
fn tchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}
fn ows(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii_start().trim_ascii_end()
}

pub fn values<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = &'a [u8]> {
    headers
        .iter()
        .filter(move |h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_slice())
}
pub fn value<'a>(headers: &'a [Header], name: &'a str) -> Option<&'a [u8]> {
    values(headers, name).next()
}

pub fn line<R: BufRead>(reader: &mut R, budget: &mut usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|_| bad("incomplete HTTP message"))?;
        if chunk.is_empty() {
            return Err(bad("incomplete HTTP message"));
        }
        let n = memchr::memchr(b'\n', chunk)
            .map(|i| i + 1)
            .unwrap_or(chunk.len());
        *budget = budget
            .checked_sub(n)
            .ok_or_else(|| bad("HTTP header is too large"))?;
        let ended = chunk[n - 1] == b'\n';
        out.extend_from_slice(&chunk[..n]);
        reader.consume(n);
        if ended {
            if out.len() < 2 || !out.ends_with(b"\r\n") {
                return Err(bad("invalid HTTP line ending"));
            }
            out.truncate(out.len() - 2);
            if out.contains(&b'\r') {
                return Err(bad("invalid HTTP line ending"));
            }
            return Ok(out);
        }
    }
}

pub fn field(bytes: &[u8]) -> Result<Header> {
    let colon = bytes
        .iter()
        .position(|&c| c == b':')
        .ok_or_else(|| bad("invalid header field"))?;
    if !token(&bytes[..colon])
        || bytes[colon + 1..]
            .iter()
            .any(|&c| (c < 32 && c != b'\t') || c == 127)
    {
        return Err(bad("invalid header field"));
    }
    Ok(Header {
        name: String::from_utf8(bytes[..colon].to_vec()).unwrap(),
        value: ows(&bytes[colon + 1..]).to_vec(),
    })
}

fn headers<R: BufRead>(reader: &mut R, budget: &mut usize) -> Result<Vec<Header>> {
    let mut headers = Vec::new();
    loop {
        let raw = line(reader, budget)?;
        if raw.is_empty() {
            return Ok(headers);
        }
        if headers.len() >= 256 {
            return Err(bad("too many headers"));
        }
        headers.push(field(&raw)?);
    }
}

pub fn request<R: BufRead>(reader: &mut R) -> Result<Request> {
    let mut budget = BUFFER;
    let mut start = line(reader, &mut budget)?;
    for _ in 0..3 {
        if !start.is_empty() {
            break;
        }
        start = line(reader, &mut budget)?;
    }
    let text = std::str::from_utf8(&start).map_err(|_| bad("invalid request line"))?;
    let parts: Vec<&str> = text.split(' ').collect();
    if parts.len() != 3 || !token(parts[0].as_bytes()) || parts[2] != "HTTP/1.1" {
        return Err(bad("invalid HTTP/1.1 request line"));
    }
    let target = parts[1];
    if target.is_empty() || target.bytes().any(|c| c <= 32 || c >= 127 || c == b'#') {
        return Err(bad("invalid request target"));
    }
    let path = if target.starts_with('/') {
        target.to_string()
    } else {
        let rest = target
            .strip_prefix("http://")
            .or_else(|| target.strip_prefix("https://"))
            .ok_or_else(|| bad("invalid request target"))?;
        match rest.find(['/', '?']) {
            Some(0) => return Err(bad("invalid request target")),
            Some(i) if rest.as_bytes()[i] == b'?' => format!("/{}", &rest[i..]),
            Some(i) => rest[i..].to_string(),
            None if !rest.is_empty() => "/".to_string(),
            _ => return Err(bad("invalid request target")),
        }
    };
    let headers = headers(reader, &mut budget)?;
    if values(&headers, "host").count() != 1
        || value(&headers, "host").is_some_and(|v| v.is_empty())
    {
        return Err(bad("request must contain exactly one Host header"));
    }
    if values(&headers, "content-type").count() > 1 {
        return Err(bad("ambiguous Content-Type"));
    }
    Ok(Request {
        method: parts[0].to_string(),
        path,
        headers,
    })
}

pub fn response<R: BufRead>(reader: &mut R) -> Result<Response> {
    loop {
        let mut budget = BUFFER;
        let start = line(reader, &mut budget)?;
        let text = std::str::from_utf8(&start).map_err(|_| bad("invalid backend response"))?;
        let status: u16 = text
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .filter(|s| (100..600).contains(s))
            .ok_or_else(|| bad("invalid backend response"))?;
        let headers = headers(reader, &mut budget)?;
        if status >= 200 {
            return Ok(Response {
                line: start,
                status,
                headers,
            });
        }
    }
}

fn spool() -> Result<Spool> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = std::env::temp_dir().join(format!(
            "switchyard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(path).map_err(Fault::io)?;
                return Ok(Spool { file, size: 0 });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Fault::io(e)),
        }
    }
}

pub fn copy_exact<R: Read, W: Write>(reader: &mut R, writer: &mut W, n: u64) -> Result<()> {
    let copied =
        std::io::copy(&mut reader.take(n), writer).map_err(|_| bad("incomplete message body"))?;
    if copied != n {
        return Err(bad("incomplete message body"));
    }
    Ok(())
}

/// How a message body is delimited (RFC 9112 6.3).
pub enum Framing {
    Length(u64),
    Chunked,
    /// No length information: empty for a request, read until close for a response.
    Unframed,
}

pub fn framing(headers: &[Header]) -> Result<Framing> {
    let lengths: Vec<_> = values(headers, "content-length").collect();
    let encodings: Vec<_> = values(headers, "transfer-encoding").collect();
    if lengths.len() > 1 || (!lengths.is_empty() && !encodings.is_empty()) {
        return Err(bad("ambiguous message framing"));
    }
    if !encodings.is_empty() {
        let codings: Vec<_> = encodings
            .iter()
            .flat_map(|v| v.split(|&c| c == b','))
            .map(ows)
            .filter(|v| !v.is_empty())
            .collect();
        if codings.len() != 1 || !codings[0].eq_ignore_ascii_case(b"chunked") {
            return Err(bad("invalid Transfer-Encoding"));
        }
        return Ok(Framing::Chunked);
    }
    if let Some(length) = lengths.first() {
        if length.is_empty() || !length.iter().all(u8::is_ascii_digit) {
            return Err(bad("invalid Content-Length"));
        }
        let n = std::str::from_utf8(length)
            .unwrap()
            .parse::<u64>()
            .map_err(|_| Fault::large())?;
        return Ok(Framing::Length(n));
    }
    Ok(Framing::Unframed)
}

/// Copies one message body to writer, decoding chunked transfer coding. The
/// reader is left exactly at the end of the message, so the next message on a
/// persistent connection starts at its first byte.
pub fn body<R: BufRead, W: Write>(
    reader: &mut R,
    headers: &[Header],
    writer: &mut W,
    limit: u64,
    request: bool,
) -> Result<u64> {
    match framing(headers)? {
        Framing::Chunked => {
            let mut total = 0;
            loop {
                let mut budget = BUFFER;
                let n = chunk_size(&line(reader, &mut budget)?)?;
                if n > limit - total {
                    return Err(Fault::large());
                }
                if n == 0 {
                    break;
                }
                copy_exact(reader, writer, n)?;
                total += n;
                let mut crlf = [0; 2];
                reader
                    .read_exact(&mut crlf)
                    .map_err(|_| bad("incomplete chunk"))?;
                if crlf != *b"\r\n" {
                    return Err(bad("invalid chunk delimiter"));
                }
            }
            let mut budget = BUFFER;
            loop {
                let trailer = line(reader, &mut budget)?;
                if trailer.is_empty() {
                    break;
                }
                field(&trailer)?;
            }
            Ok(total)
        }
        Framing::Length(n) => {
            if n > limit {
                return Err(Fault::large());
            }
            copy_exact(reader, writer, n)?;
            Ok(n)
        }
        Framing::Unframed if request => Ok(0),
        Framing::Unframed => {
            let mut buffer = [0; BUFFER];
            let mut total = 0;
            loop {
                let n = reader.read(&mut buffer).map_err(Fault::io)?;
                if n == 0 {
                    return Ok(total);
                }
                if n as u64 > limit - total {
                    return Err(Fault::large());
                }
                writer.write_all(&buffer[..n]).map_err(Fault::io)?;
                total += n as u64;
            }
        }
    }
}

/// Writes each block it is given as one chunk, so a re-framed response still
/// reaches the client as soon as its bytes arrive. finish() ends the body.
pub struct Chunked<'a, W: Write>(pub &'a mut W);

impl<W: Write> Write for Chunked<'_, W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.is_empty() {
            return Ok(0); // a zero-size chunk would end the body
        }
        let mut frame = Vec::with_capacity(data.len() + 20);
        write!(frame, "{:x}\r\n", data.len())?;
        frame.extend_from_slice(data);
        frame.extend_from_slice(b"\r\n");
        self.0.write_all(&frame)?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> Chunked<'_, W> {
    pub fn finish(self) -> std::io::Result<()> {
        self.0.write_all(b"0\r\n\r\n")
    }
}

pub fn request_body<R: BufRead>(reader: &mut R, headers: &[Header], limit: u64) -> Result<Spool> {
    let mut spool = spool()?;
    spool.size = body(reader, headers, &mut spool.file, limit, true)?;
    Ok(spool)
}

pub fn chunk_size(line: &[u8]) -> Result<u64> {
    let end = line
        .iter()
        .position(|c| !c.is_ascii_hexdigit())
        .unwrap_or(line.len());
    if end == 0 || !valid_extensions(&line[end..]) {
        return Err(bad("invalid chunk header"));
    }
    u64::from_str_radix(std::str::from_utf8(&line[..end]).unwrap(), 16).map_err(|_| Fault::large())
}

fn skip_ows(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], b' ' | b'\t') {
        i += 1;
    }
    i
}
fn token_end(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && tchar(s[i]) {
        i += 1;
    }
    i
}
fn quoted_end(s: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < s.len() {
        match s[i] {
            b'"' => return Some(i + 1),
            b'\\' => {
                i += 1;
                if i == s.len() || (s[i] < 32 && s[i] != b'\t') || s[i] == 127 {
                    return None;
                }
            }
            c if (c < 32 && c != b'\t') || c == 127 => return None,
            _ => {}
        }
        i += 1;
    }
    None
}
fn valid_extensions(s: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len() {
        i = skip_ows(s, i);
        if s.get(i) != Some(&b';') {
            return false;
        }
        i = skip_ows(s, i + 1);
        let end = token_end(s, i);
        if end == i {
            return false;
        }
        i = end;
        let next = skip_ows(s, i);
        if s.get(next) == Some(&b'=') {
            i = skip_ows(s, next + 1);
            let end = if s.get(i) == Some(&b'"') {
                quoted_end(s, i)
            } else {
                Some(token_end(s, i))
            };
            match end {
                Some(end) if end > i => i = end,
                _ => return false,
            }
        }
    }
    true
}

pub fn parameters(raw: &[u8]) -> Result<(String, std::collections::HashMap<String, String>)> {
    let s = std::str::from_utf8(raw).map_err(|_| bad("invalid media type"))?;
    let bytes = s.as_bytes();
    let split = s.find(';').unwrap_or(s.len());
    let name = s[..split].trim().to_ascii_lowercase();
    let mut params = std::collections::HashMap::new();
    let mut i = split;
    while i < bytes.len() {
        if bytes[i] != b';' {
            return Err(bad("invalid media parameter"));
        }
        i = skip_ows(bytes, i + 1);
        if i == bytes.len() {
            break;
        }
        // HTTP media-type parameters may contain empty semicolon entries.
        if bytes[i] == b';' {
            continue;
        }
        let end = token_end(bytes, i);
        if end == i {
            return Err(bad("invalid media parameter"));
        }
        let key = s[i..end].to_ascii_lowercase();
        i = skip_ows(bytes, end);
        if bytes.get(i) != Some(&b'=') {
            return Err(bad("invalid media parameter"));
        }
        i = skip_ows(bytes, i + 1);
        let mut value = Vec::new();
        if bytes.get(i) == Some(&b'"') {
            let end = quoted_end(bytes, i).ok_or_else(|| bad("invalid quoted parameter"))?;
            i += 1;
            while i < end - 1 {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                value.push(bytes[i]);
                i += 1;
            }
            i = end;
        } else {
            let end = token_end(bytes, i);
            if end == i {
                return Err(bad("invalid media parameter"));
            }
            value.extend_from_slice(&bytes[i..end]);
            i = end;
        }
        let value = String::from_utf8(value).map_err(|_| bad("invalid media parameter"))?;
        if params.insert(key, value).is_some() {
            return Err(bad("duplicate media parameter"));
        }
        i = skip_ows(bytes, i);
    }
    Ok((name, params))
}

pub fn hop_headers(headers: &[Header]) -> HashSet<String> {
    let fixed = [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    let mut hop: HashSet<String> = fixed.into_iter().map(str::to_owned).collect();
    for v in values(headers, "connection") {
        for name in v.split(|&c| c == b',') {
            if let Ok(name) = std::str::from_utf8(ows(name)) {
                hop.insert(name.to_ascii_lowercase());
            }
        }
    }
    hop
}
