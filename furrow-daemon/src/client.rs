//! One local request. A failed exchange is never replayed automatically.

use crate::wire::Response;
use anyhow::Context;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;
#[cfg(windows)]
use uds_windows::UnixStream;

pub fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(socket)
}

pub fn exchange(mut stream: UnixStream, request: &Value) -> anyhow::Result<Value> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let answer: Response = serde_json::from_str(&line).context("read daemon response")?;
    anyhow::ensure!(
        answer.v == crate::wire::VERSION && answer.id == request["id"],
        "daemon response does not match request"
    );
    if let Some(error) = answer.err {
        anyhow::bail!("{error}");
    }
    answer.ok.context("daemon response has no result")
}
