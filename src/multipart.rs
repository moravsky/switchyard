use crate::window::Window;
use crate::{bad, http, Model, Result, BUFFER};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub fn valid_boundary(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 70
        && !s.ends_with(' ')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(&c))
}

// A matched boundary is a delimiter only when its suffix completes a delimiter line.
fn delimiter<R: Read>(input: &mut Window<R>, n: usize) -> Result<u8> {
    if input.ensure(n + 2)? < n + 2 {
        return Ok(0);
    }
    if &input.buf[input.pos + n..input.pos + n + 2] == b"--" {
        input.pos += n + 2;
        return Ok(2);
    }
    let mut i = n;
    loop {
        if input.ensure(i + 2)? < i + 2 {
            return Ok(0);
        }
        match input.buf[input.pos + i] {
            b' ' | b'\t' => i += 1,
            b'\r' if input.buf[input.pos + i + 1] == b'\n' => {
                input.pos += i + 2;
                return Ok(1);
            }
            _ => return Ok(0),
        }
    }
}

fn next<R: Read>(input: &mut Window<R>, pattern: &[u8]) -> Result<(u8, u64)> {
    while input.find(pattern)? {
        let offset = input.offset();
        let kind = delimiter(input, pattern.len())?;
        if kind != 0 {
            return Ok((kind, offset));
        }
        input.pos += 1;
    }
    Err(bad("unterminated multipart body"))
}

fn field_name<R: Read>(input: &mut Window<R>) -> Result<Option<String>> {
    let mut budget = BUFFER;
    let mut fields: Vec<Vec<u8>> = Vec::new();
    loop {
        let line = input.line(&mut budget)?;
        if line.is_empty() {
            break;
        }
        if matches!(line[0], b' ' | b'\t') {
            fields
                .last_mut()
                .ok_or_else(|| bad("invalid multipart header"))?
                .extend_from_slice(&line);
        } else {
            fields.push(line);
        }
    }
    let mut seen = false;
    let mut name = None;
    for raw in fields {
        let field = http::field(&raw)?;
        if field.name.eq_ignore_ascii_case("content-disposition") {
            if seen {
                return Err(bad("ambiguous multipart disposition"));
            }
            seen = true;
            let (kind, params) = http::parameters(&field.value)?;
            if kind == "form-data" {
                name = params.get("name").cloned();
            }
        }
    }
    Ok(name)
}

pub fn scan(file: &mut File, boundary: &str, cap: usize) -> Result<Model> {
    file.seek(SeekFrom::Start(0)).map_err(crate::Fault::io)?;
    let dash = format!("--{boundary}").into_bytes();
    let pattern = format!("\r\n--{boundary}").into_bytes();
    let mut input = Window::new(&mut *file);
    let mut kind = 0;
    if input.ensure(dash.len())? >= dash.len() && input.buf[..dash.len()] == dash {
        kind = delimiter(&mut input, dash.len())?;
    }
    if kind == 0 {
        kind = next(&mut input, &pattern)?.0;
    }
    if kind != 1 {
        return Err(bad("multipart body has no parts"));
    }
    let mut model = Model::default();
    while kind == 1 {
        let name = field_name(&mut input)?;
        let start = input.offset();
        let (next_kind, end) = next(&mut input, &pattern)?;
        kind = next_kind;
        if name.as_deref() == Some("model") {
            if model.present {
                return Err(bad("duplicate model form field"));
            }
            model.present = true;
            model.range = Some((start, end));
        }
    }
    drop(input);
    if let Some((start, end)) = model.range {
        model.over = end - start > cap as u64;
        model
            .value
            .resize((end - start).min(cap as u64) as usize, 0);
        file.seek(SeekFrom::Start(start))
            .map_err(crate::Fault::io)?;
        file.read_exact(&mut model.value)
            .map_err(crate::Fault::io)?;
    }
    Ok(model)
}
