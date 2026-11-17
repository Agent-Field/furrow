//! The wire: one JSON object per line, request id and `V` on every message
//! (law L11), the same envelope shape as the harness's own wire.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

/// The protocol version carried by every message.
pub const VERSION: u16 = 1;

/// The longest frame read: a longer line is a broken or hostile peer.
const MAX_FRAME: u64 = 16 << 20;

/// The store a request works on: the engine's data directory, the tree it
/// seals, and the directory composed in as the tree's `.cell/` entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct Target {
    pub data_dir: PathBuf,
    pub tree: PathBuf,
    #[serde(default)]
    pub cell_dir: Option<PathBuf>,
    #[serde(default)]
    pub timeline: Option<String>,
    /// The client's `GIT_CEILING_DIRECTORIES`, bounding Git's policy
    /// discovery exactly as the client's own environment would: absent keeps
    /// the daemon's environment (older clients), an empty value is no ceiling
    /// at all. It is part of `Target` because `Target` keys the retained
    /// repositories, and a different ceiling means different Git rules.
    #[serde(default)]
    pub ceiling: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(rename = "V")]
    pub v: u16,
    pub id: u64,
    pub verb: String,
    #[serde(default)]
    pub target: Option<Target>,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Response {
    #[serde(rename = "V")]
    pub v: u16,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

impl Response {
    pub fn ok(id: u64, value: Value) -> Self {
        Self {
            v: VERSION,
            id,
            ok: Some(value),
            err: None,
        }
    }

    pub fn err(id: u64, message: String) -> Self {
        Self {
            v: VERSION,
            id,
            ok: None,
            err: Some(message),
        }
    }
}

/// Reads the next line. `None` is a clean end of stream; a final line without
/// its newline is a torn write and is not a request.
pub fn read_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let read = reader.take(MAX_FRAME).read_line(&mut line)?;
    if read == 0 || !line.ends_with('\n') {
        return Ok(None);
    }
    Ok(Some(line))
}

pub fn write_response(writer: &mut impl Write, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// The answer to a line, whatever the line was: a malformed request is an
/// error response with id 0, never a dropped connection.
pub fn parse_request(line: &str) -> Result<Request, Response> {
    let request: Request =
        serde_json::from_str(line).map_err(|e| Response::err(0, format!("bad request: {e}")))?;
    if request.v != VERSION {
        return Err(Response::err(
            request.id,
            format!("protocol V {} not supported (V {VERSION})", request.v),
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceiling rides the wire Target: absent keeps the daemon's
    /// environment for older clients, an empty value is no ceiling at all,
    /// and a value names the client's own `GIT_CEILING_DIRECTORIES`.
    #[test]
    fn w1_ceiling_wire_semantics() {
        let seal = |target: serde_json::Value| {
            let mut request = serde_json::json!({"V": 1, "id": 1, "verb": "seal", "args": {}});
            request["target"] = target;
            request.to_string()
        };
        let target = |ceiling: Option<&str>| {
            let mut target = serde_json::json!({"data_dir": "/d", "tree": "/t"});
            if let Some(ceiling) = ceiling {
                target["ceiling"] = serde_json::Value::String(ceiling.to_owned());
            }
            target
        };
        let request = parse_request(&seal(target(None))).unwrap();
        assert_eq!(
            request.target.as_ref().unwrap().ceiling,
            None,
            "absent ceiling keeps the daemon's environment"
        );
        let request = parse_request(&seal(target(Some("")))).unwrap();
        assert_eq!(
            request.target.as_ref().unwrap().ceiling,
            Some(String::new()),
            "an empty ceiling is no ceiling at all"
        );
        let request = parse_request(&seal(target(Some("/x/boundary")))).unwrap();
        assert_eq!(
            request.target.as_ref().unwrap().ceiling,
            Some("/x/boundary".to_owned())
        );
    }
}
