mod config;
mod http;
mod json;
mod multipart;
mod window;

use config::{Backend, Config};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BUFFER: usize = 64 * 1024;
/// How long a client may stay silent mid-request, or idle between requests on
/// a persistent connection, and how long a write to it may block.
const CLIENT_IDLE: Duration = Duration::from_secs(60);
const CONNECT: Duration = Duration::from_secs(60);
/// Model backends may think for minutes before or between response bytes; the
/// contract requires waiting at least 10 minutes for each.
const BACKEND_WAIT: Duration = Duration::from_secs(15 * 60);
type Result<T> = std::result::Result<T, Fault>;
#[derive(Debug)]
struct Fault {
    status: u16,
    message: &'static str,
}
impl Fault {
    fn io(_: std::io::Error) -> Self {
        Self {
            status: 500,
            message: "gateway I/O error",
        }
    }
    fn large() -> Self {
        Self {
            status: 413,
            message: "request body exceeds max_body",
        }
    }
}
fn bad(message: &'static str) -> Fault {
    Fault {
        status: 400,
        message,
    }
}
fn unreachable() -> Fault {
    Fault {
        status: 502,
        message: "backend unavailable",
    }
}
#[derive(Default)]
struct Model {
    present: bool,
    value: Vec<u8>,
    over: bool,
    range: Option<(u64, u64)>,
}

fn connect(backend: &Backend, timeout: Duration, io: Duration) -> Result<TcpStream> {
    let addresses = (backend.host.as_str(), backend.port)
        .to_socket_addrs()
        .map_err(|_| unreachable())?;
    let deadline = Instant::now() + timeout;
    for address in addresses {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if let Ok(socket) = TcpStream::connect_timeout(&address, remaining) {
            socket.set_nodelay(true).ok();
            socket.set_read_timeout(Some(io)).ok();
            socket.set_write_timeout(Some(io)).ok();
            return Ok(socket);
        }
    }
    Err(unreachable())
}

fn connection_header(close: bool) -> &'static str {
    if close {
        "Connection: close\r\n"
    } else {
        ""
    }
}

fn error(client: &mut TcpStream, fault: Fault, close: bool) -> std::io::Result<()> {
    let data = serde_json::to_vec(&serde_json::json!({"error": {"message": fault.message, "type": "switchyard_error", "code": fault.status}})).unwrap();
    let mut out = format!(
        "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n",
        fault.status,
        data.len(),
        connection_header(close)
    )
    .into_bytes();
    out.extend_from_slice(&data);
    client.write_all(&out)
}

fn inventory(backend: &Backend) -> Option<serde_json::Value> {
    let timeout = Duration::from_secs(2);
    let deadline = Instant::now() + timeout;
    let mut socket = connect(backend, timeout, timeout).ok()?;
    write!(
        socket,
        "GET {}/v1/models HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        backend.base, backend.authority
    )
    .ok()?;
    let mut reader = BufReader::with_capacity(BUFFER, Deadline { socket, deadline });
    let response = http::response(&mut reader).ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let mut bytes = Vec::new();
    http::body(
        &mut reader,
        &response.headers,
        &mut bytes,
        1024 * 1024,
        false,
    )
    .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let mut entry = value
        .get("data")?
        .as_array()?
        .iter()
        .find(|v| v.is_object())?
        .clone();
    entry
        .as_object_mut()?
        .insert("id".to_owned(), backend.name.clone().into());
    Some(entry)
}

struct Deadline {
    socket: TcpStream,
    deadline: Instant,
}

impl Read for Deadline {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "backend deadline"))?;
        self.socket.set_read_timeout(Some(remaining))?;
        self.socket.read(bytes)
    }
}

fn models(client: &mut TcpStream, config: &Config, close: bool) -> Result<After> {
    let entries = std::thread::scope(|scope| {
        let jobs: Vec<_> = config
            .backends
            .iter()
            .map(|backend| scope.spawn(move || inventory(backend)))
            .collect();
        jobs.into_iter()
            .filter_map(|j| j.join().ok().flatten())
            .collect::<Vec<_>>()
    });
    let data = serde_json::to_vec(&serde_json::json!({"object": "list", "data": entries})).unwrap();
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n",
        data.len(),
        connection_header(close)
    )
    .into_bytes();
    out.extend_from_slice(&data);
    // The answer is already final; a failed write only ends the connection.
    Ok(match client.write_all(&out) {
        Ok(()) => After::Keep,
        Err(_) => After::Close,
    })
}

/// Whether a connection can carry another request after this exchange.
#[derive(Clone, Copy, PartialEq)]
enum After {
    Keep,
    Close,
}

fn relay(
    client: &mut TcpStream,
    request: &http::Request,
    spool: &mut http::Spool,
    model: Model,
    is_json: bool,
    backend: &Backend,
    close: bool,
) -> Result<After> {
    let mut upstream = connect(backend, CONNECT, BACKEND_WAIT)?;
    let rewrite = match (model.range, backend.upstream.as_ref()) {
        (Some(range), Some(value)) => Some((
            range,
            if is_json {
                serde_json::to_vec(value).unwrap()
            } else {
                value.as_bytes().to_vec()
            },
        )),
        _ => None,
    };
    let length = match &rewrite {
        Some(((start, end), value)) => spool.size - (end - start) + value.len() as u64,
        None => spool.size,
    };
    write!(
        upstream,
        "{} {}{} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        request.method, backend.base, request.path, backend.authority, length
    )
    .map_err(|_| unreachable())?;
    let hop = http::hop_headers(&request.headers);
    for header in &request.headers {
        let name = header.name.to_ascii_lowercase();
        if hop.contains(&name) || ["host", "content-length", "expect"].contains(&name.as_str()) {
            continue;
        }
        upstream
            .write_all(header.name.as_bytes())
            .and_then(|_| upstream.write_all(b": "))
            .and_then(|_| upstream.write_all(&header.value))
            .and_then(|_| upstream.write_all(b"\r\n"))
            .map_err(|_| unreachable())?;
    }
    upstream.write_all(b"\r\n").map_err(|_| unreachable())?;
    spool.file.seek(SeekFrom::Start(0)).map_err(Fault::io)?;
    if let Some(((start, end), value)) = rewrite {
        http::copy_exact(&mut spool.file, &mut upstream, start)?;
        upstream.write_all(&value).map_err(|_| unreachable())?;
        spool.file.seek(SeekFrom::Start(end)).map_err(Fault::io)?;
    }
    std::io::copy(&mut spool.file, &mut upstream).map_err(|_| unreachable())?;
    let mut reader = BufReader::with_capacity(BUFFER, upstream);
    let response = http::response(&mut reader).map_err(|_| unreachable())?;
    let framing = http::framing(&response.headers).map_err(|_| unreachable())?;
    let bodiless = request.method == "HEAD" || response.status == 204 || response.status == 304;
    // Every response must be self-delimiting so the connection can carry the
    // next request: a known length passes through, anything else is re-sent
    // chunked. Once the head is written, a failure can only end the connection.
    let mut head = b"HTTP/1.1 ".to_vec();
    head.extend_from_slice(
        response
            .line
            .splitn(2, |&c| c == b' ')
            .nth(1)
            .unwrap_or_default(),
    );
    head.extend_from_slice(b"\r\n");
    let hop = http::hop_headers(&response.headers);
    for header in &response.headers {
        if hop.contains(&header.name.to_ascii_lowercase())
            || header.name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        head.extend_from_slice(header.name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(&header.value);
        head.extend_from_slice(b"\r\n");
    }
    match framing {
        http::Framing::Length(n) if response.status != 204 => {
            head.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes())
        }
        http::Framing::Length(_) => {}
        _ if bodiless => {}
        _ => head.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
    }
    head.extend_from_slice(connection_header(close).as_bytes());
    head.extend_from_slice(b"\r\n");
    if client.write_all(&head).is_err() {
        return Ok(After::Close);
    }
    if bodiless {
        return Ok(After::Keep);
    }
    let complete = match framing {
        http::Framing::Length(_) => {
            http::body(&mut reader, &response.headers, client, u64::MAX, false).is_ok()
        }
        _ => {
            let mut chunked = http::Chunked(client);
            http::body(&mut reader, &response.headers, &mut chunked, u64::MAX, false).is_ok()
                && chunked.finish().is_ok()
        }
    };
    Ok(if complete { After::Keep } else { After::Close })
}

/// Reads one request head and body. A failure here leaves the position of the
/// next request unknown, so the connection must close after the error
/// (RFC 9112 6.3); it is never resynchronized.
fn receive(
    client: &mut TcpStream,
    reader: &mut impl BufRead,
    config: &Config,
) -> Result<(http::Request, http::Spool)> {
    let request = http::request(reader)?;
    if http::values(&request.headers, "expect").any(|v| v.eq_ignore_ascii_case(b"100-continue")) {
        // Answer the expectation before reading content: the final 413 when the
        // declared length already exceeds max_body, otherwise 100 Continue.
        let lengths: Vec<_> = http::values(&request.headers, "content-length").collect();
        if let [length] = lengths.as_slice() {
            if !length.is_empty() && length.iter().all(u8::is_ascii_digit) {
                let n = std::str::from_utf8(length)
                    .unwrap()
                    .parse::<u64>()
                    .unwrap_or(u64::MAX);
                if n > config.max_body {
                    return Err(Fault::large());
                }
            }
        }
        client
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(Fault::io)?;
    }
    let spool = http::request_body(reader, &request.headers, config.max_body)?;
    Ok((request, spool))
}

/// Routes one fully received request. Errors here are answered without
/// ending the connection: the request was framed correctly and fully read.
fn handle(
    client: &mut TcpStream,
    request: &http::Request,
    spool: &mut http::Spool,
    config: &Config,
    close: bool,
) -> Result<After> {
    let raw_type = http::value(&request.headers, "content-type").unwrap_or(b"");
    let media_type = raw_type
        .split(|&c| c == b';')
        .next()
        .unwrap_or(b"")
        .trim_ascii();
    let is_json = media_type.eq_ignore_ascii_case(b"application/json");
    // An empty body carries no model, whatever its declared media type.
    let model = if spool.size == 0 {
        Model::default()
    } else if is_json {
        spool.file.seek(SeekFrom::Start(0)).map_err(Fault::io)?;
        json::scan(&mut spool.file, config.model_cap)?
    } else if media_type.eq_ignore_ascii_case(b"multipart/form-data") {
        let (_, params) = http::parameters(raw_type)?;
        let boundary = params
            .get("boundary")
            .filter(|b| multipart::valid_boundary(b))
            .ok_or_else(|| bad("invalid multipart boundary"))?;
        multipart::scan(&mut spool.file, boundary, config.model_cap)?
    } else {
        Model::default()
    };
    if request.method == "GET" && request.path.split('?').next() == Some("/v1/models") {
        return models(client, config, close);
    }
    let index = if model.present {
        if model.over {
            None
        } else {
            config.routes.get(&model.value).copied()
        }
        .ok_or(Fault {
            status: 404,
            message: "unknown model",
        })?
    } else {
        config
            .default
            .ok_or_else(|| bad("request has no model and no default is configured"))?
    };
    relay(
        client,
        request,
        spool,
        model,
        is_json,
        &config.backends[index],
        close,
    )
}

fn wants_close(headers: &[http::Header]) -> bool {
    http::values(headers, "connection")
        .flat_map(|v| v.split(|&c| c == b','))
        .any(|option| option.trim_ascii().eq_ignore_ascii_case(b"close"))
}

/// Serves one request on the connection and reports whether it stays open.
fn exchange(client: &mut TcpStream, reader: &mut impl BufRead, config: &Config) -> After {
    let (request, mut spool) = match receive(client, reader, config) {
        Ok(received) => received,
        Err(fault) => {
            let _ = error(client, fault, true);
            return After::Close;
        }
    };
    let close = wants_close(&request.headers);
    let after = match handle(client, &request, &mut spool, config, close) {
        Ok(after) => after,
        Err(fault) => match error(client, fault, close) {
            Ok(()) => After::Keep,
            Err(_) => After::Close,
        },
    };
    if close {
        After::Close
    } else {
        after
    }
}

/// Handles requests on one client connection in order, one at a time, until
/// the client closes or asks to, goes idle, or sends a message whose framing
/// cannot be trusted. One reader spans the whole connection, so bytes of a
/// pipelined request that arrived early stay buffered for the next exchange.
fn serve(mut client: TcpStream, config: &Config) {
    client.set_read_timeout(Some(CLIENT_IDLE)).ok();
    client.set_write_timeout(Some(CLIENT_IDLE)).ok();
    client.set_nodelay(true).ok();
    let Ok(socket) = client.try_clone() else {
        return;
    };
    let mut reader = BufReader::with_capacity(BUFFER, socket);
    loop {
        // End quietly when the client closes or idles between requests.
        if !matches!(reader.fill_buf(), Ok(bytes) if !bytes.is_empty()) {
            return;
        }
        if exchange(&mut client, &mut reader, config) == After::Close {
            break;
        }
    }
    // Deliver the final response even when the peer is still uploading a
    // rejected body: half-close, then drain briefly before closing.
    let _ = client.shutdown(Shutdown::Write);
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    let mut buffer = [0; BUFFER];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match client.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let loaded = if args.len() == 2 {
        std::fs::read_to_string(&args[1])
            .map_err(Fault::io)
            .and_then(|raw| config::load(&raw))
    } else {
        Err(bad("usage: gateway <config.toml>"))
    };
    let config = match loaded {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("{}", e.message);
            std::process::exit(1);
        }
    };
    let listener = match TcpListener::bind(&config.listen) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("cannot bind listen address");
            std::process::exit(1);
        }
    };
    for incoming in listener.incoming() {
        let Ok(client) = incoming else {
            continue;
        };
        let config = config.clone();
        std::thread::spawn(move || serve(client, &config));
    }
}
