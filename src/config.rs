use crate::{bad, Result};
use std::collections::HashMap;

pub struct Backend {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub authority: String,
    pub base: String,
    pub upstream: Option<String>,
}
pub struct Config {
    pub listen: String,
    pub default: Option<usize>,
    pub max_body: u64,
    pub backends: Vec<Backend>,
    pub routes: HashMap<Vec<u8>, usize>,
    pub model_cap: usize,
}

fn text(table: &toml::map::Map<String, toml::Value>, key: &str) -> Result<Option<String>> {
    table
        .get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad("config field must be a string"))
        })
        .transpose()
}

fn url(raw: &str) -> Result<(String, u16, String, String)> {
    let rest = raw
        .strip_prefix("http://")
        .ok_or_else(|| bad("backend URL must use http"))?;
    if rest
        .bytes()
        .any(|c| c <= 32 || c >= 127 || matches!(c, b'?' | b'#' | b'\\' | b'@'))
    {
        return Err(bad("invalid backend URL"));
    }
    let (authority, base) = rest
        .find('/')
        .map(|i| (&rest[..i], &rest[i..]))
        .unwrap_or((rest, ""));
    let (host, port) = if let Some(ip) = authority.strip_prefix('[') {
        let end = ip.find(']').ok_or_else(|| bad("invalid backend host"))?;
        let host = &ip[..end];
        host.parse::<std::net::Ipv6Addr>()
            .map_err(|_| bad("invalid backend host"))?;
        let tail = &ip[end + 1..];
        (
            host,
            if tail.is_empty() {
                None
            } else {
                Some(
                    tail.strip_prefix(':')
                        .ok_or_else(|| bad("invalid backend port"))?,
                )
            },
        )
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() || host.bytes().any(|c| matches!(c, b'[' | b']' | b'%' | b'/')) {
        return Err(bad("invalid backend host"));
    }
    let port = match port {
        Some(p) if !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()) => p
            .parse::<u16>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| bad("invalid backend port"))?,
        Some(_) => return Err(bad("invalid backend port")),
        None => 80,
    };
    // Percent escapes in a configured path must be well formed; leave valid escapes intact.
    let mut i = 0;
    while i < base.len() {
        if base.as_bytes()[i] == b'%' {
            if i + 2 >= base.len()
                || !base.as_bytes()[i + 1..i + 3]
                    .iter()
                    .all(u8::is_ascii_hexdigit)
            {
                return Err(bad("invalid backend path"));
            }
            i += 2;
        }
        i += 1;
    }
    Ok((
        host.to_owned(),
        port,
        authority.to_owned(),
        base.trim_end_matches('/').to_owned(),
    ))
}

pub fn load(raw: &str) -> Result<Config> {
    let root: toml::Value = toml::from_str(raw).map_err(|_| bad("invalid TOML config"))?;
    let root = root
        .as_table()
        .ok_or_else(|| bad("config must be a table"))?;
    if root
        .keys()
        .any(|k| !["listen", "default", "max_body", "backend"].contains(&k.as_str()))
    {
        return Err(bad("unknown config field"));
    }
    let listen = text(root, "listen")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("missing listen address"))?;
    let default_name = text(root, "default")?;
    let max_body = match root.get("max_body") {
        Some(v) => v
            .as_integer()
            .filter(|&n| n >= 0)
            .ok_or_else(|| bad("invalid max_body"))? as u64,
        None => 256 * 1024 * 1024,
    };
    let entries = root
        .get("backend")
        .and_then(toml::Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| bad("config has no backends"))?;
    let mut cfg = Config {
        listen,
        default: None,
        max_body,
        backends: Vec::new(),
        routes: HashMap::new(),
        model_cap: 1,
    };
    for (index, entry) in entries.iter().enumerate() {
        let table = entry
            .as_table()
            .ok_or_else(|| bad("backend must be a table"))?;
        if table
            .keys()
            .any(|k| !["name", "url", "aliases", "upstream_model"].contains(&k.as_str()))
        {
            return Err(bad("unknown backend field"));
        }
        let name = text(table, "name")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("backend has no name"))?;
        let raw_url = text(table, "url")?.ok_or_else(|| bad("backend has no URL"))?;
        let (host, port, authority, base) = url(&raw_url)?;
        let mut names = vec![name.clone()];
        if let Some(aliases) = table.get("aliases") {
            for alias in aliases
                .as_array()
                .ok_or_else(|| bad("aliases must be an array"))?
            {
                names.push(
                    alias
                        .as_str()
                        .ok_or_else(|| bad("alias must be a string"))?
                        .to_owned(),
                );
            }
        }
        for key in names {
            cfg.model_cap = cfg.model_cap.max(key.len());
            if cfg
                .routes
                .insert(key.into_bytes(), index)
                .is_some_and(|previous| previous != index)
            {
                return Err(bad("model name belongs to multiple backends"));
            }
        }
        if default_name.as_ref() == Some(&name) {
            cfg.default = Some(index);
        }
        cfg.backends.push(Backend {
            name,
            host,
            port,
            authority,
            base,
            upstream: text(table, "upstream_model")?,
        });
    }
    if default_name.is_some() && cfg.default.is_none() {
        return Err(bad("default must name a backend"));
    }
    Ok(cfg)
}
