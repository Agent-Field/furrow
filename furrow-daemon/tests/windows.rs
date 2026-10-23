#![cfg(windows)]

use furrow_daemon::{Config, Server};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};
use uds_windows::UnixStream;

#[test]
fn w2_af_unix_health_hook_shutdown_and_idle() {
    // W2: the actual AF_UNIX wire accepts JSON, holds repositories between
    // turns, serves a clean shutdown response, and removes its socket.
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("f.sock");
    let config = Config {
        socket: socket.clone(),
        idle: Duration::from_secs(3),
    };
    let server = Server::bind(config.clone()).unwrap().unwrap();
    assert!(Server::bind(config).unwrap().is_none());
    let thread = std::thread::spawn(move || server.run().unwrap());
    let root = temp.path().join("tree");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("bytes"), b"\0\xff\r\n").unwrap();
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut request = |request: Value| {
        writeln!(writer, "{request}").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert!(response.get("err").is_none(), "{response}");
        assert_eq!(response["V"], 1);
        response
    };
    request(json!({"V": 1, "id": 1, "verb": "health"}));
    request(
        json!({"V": 1, "id": 2, "verb": "seal", "target": {"data_dir": temp.path().join("data"), "tree": root}, "args": {"turn": "one"}}),
    );
    request(json!({"V": 1, "id": 3, "verb": "shutdown"}));
    thread.join().unwrap();
    assert!(!socket.exists());
    drop(reader);
    drop(writer);
    let server = Server::bind(Config {
        socket: socket.clone(),
        idle: Duration::from_millis(100),
    })
    .unwrap()
    .unwrap();
    let start = Instant::now();
    server.run().unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(!socket.exists());
}
