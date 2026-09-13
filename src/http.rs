//! The little HTTP/1.1 server both the web panel and the Unix-socket API
//! run on: one thread per connection, a request line, headers and a query
//! string parsed by hand, a response written back. No framework — the
//! whole protocol surface is a handful of paths.

use std::io::{Read, Write};

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the request may change anything: it must carry the token
    /// (header `X-Token` or query `token`), and if a browser sent an
    /// `Origin`, that origin must be this server itself.
    pub fn authorised(&self, token: &str) -> bool {
        let presented = self
            .header("x-token")
            .or_else(|| self.get("token"))
            .unwrap_or("");
        if presented != token {
            return false;
        }
        match (self.header("origin"), self.header("host")) {
            (Some(origin), Some(host)) => origin == format!("http://{host}"),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub fn number(&self, key: &str) -> Option<u8> {
        self.get(key).and_then(|v| v.parse::<u8>().ok())
    }
}

pub(crate) fn parse<S: Read>(stream: &mut S) -> Option<Request> {
    let mut buffer = [0_u8; 8192];
    let mut data = Vec::new();
    loop {
        let n = stream.read(&mut buffer).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 65536 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data);
    let mut lines = text.lines();
    let line = lines.next()?;
    let headers = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    Some(Request {
        method,
        path: path.to_string(),
        query,
        headers,
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn respond<W: Write>(stream: &mut W, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// A JSON reply.
pub(crate) fn respond_json<W: Write>(
    stream: &mut W,
    status: &str,
    value: &impl hd60s_api::serde::Serialize,
) {
    let body = serde_json::to_vec(value).unwrap_or_default();
    respond(stream, status, "application/json", &body);
}

/// The `Host` a browser may use to reach the panel: the bind address, or a
/// loopback name with that port. Anything else is a DNS-rebinding attempt.
pub(crate) fn host_allowed(request: &Request, bind: &str) -> bool {
    let Some(host) = request.header("host") else {
        return true;
    };
    if host == bind {
        return true;
    }
    let port = bind.rsplit(':').next().unwrap_or("");
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|name| host == format!("{name}:{port}"))
}
