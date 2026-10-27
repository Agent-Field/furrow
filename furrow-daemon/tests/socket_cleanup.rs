#![cfg(unix)]

use furrow_daemon::{Config, Server};
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

fn bind(socket: &Path, idle: Duration) -> Server {
    Server::bind(Config {
        socket: socket.to_owned(),
        idle,
    })
    .unwrap()
    .expect("the socket is available")
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = fs::symlink_metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

#[test]
fn c1_idle_exit_preserves_a_replacement_socket() {
    // Contract 1 and 4 require an idle exit to leave a newer daemon's socket intact.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let first = bind(&socket, Duration::ZERO);
    let original = identity(&socket);
    fs::remove_file(&socket).unwrap();
    let lock = socket.with_extension("sock.lock");
    fs::remove_file(&lock).unwrap();
    let second = bind(&socket, Duration::from_secs(60));
    let replacement = identity(&socket);
    let replacement_lock = identity(&lock);
    assert_ne!(original, replacement);

    first.run().unwrap();

    assert_eq!(identity(&socket), replacement);
    assert_eq!(identity(&lock), replacement_lock);
    assert!(Server::bind(Config {
        socket: socket.clone(),
        idle: Duration::ZERO,
    })
    .unwrap()
    .is_none());
    drop(UnixStream::connect(&socket).unwrap());
    second.stop_handle().stop();
    second.run().unwrap();
    assert!(!socket.exists());
}

#[test]
fn c1_stopped_exit_preserves_a_replacement_file() {
    // Contract 1 and 4 require an ordinary stop to preserve a replacement file's identity and bytes.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let server = bind(&socket, Duration::from_secs(60));
    fs::rename(&socket, dir.path().join("retired.sock")).unwrap();
    fs::write(&socket, b"replacement").unwrap();
    let replacement = identity(&socket);
    server.stop_handle().stop();

    server.run().unwrap();

    assert_eq!(identity(&socket), replacement);
    assert_eq!(fs::read(&socket).unwrap(), b"replacement");
}

#[test]
fn c1_stopped_exit_preserves_a_replacement_symlink() {
    // Contract 1 requires a replacement link to remain even when it points to the original socket.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let server = bind(&socket, Duration::from_secs(60));
    let retired = dir.path().join("retired.sock");
    fs::rename(&socket, &retired).unwrap();
    symlink(&retired, &socket).unwrap();
    let replacement = identity(&socket);
    server.stop_handle().stop();

    server.run().unwrap();

    assert_eq!(identity(&socket), replacement);
    assert_eq!(fs::read_link(&socket).unwrap(), retired);
}

#[test]
fn c1_idle_exit_leaves_a_missing_socket_absent() {
    // Contract 1 requires cleanup to succeed when the socket has already been removed.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let server = bind(&socket, Duration::ZERO);
    fs::remove_file(&socket).unwrap();

    server.run().unwrap();

    assert!(!socket.exists());
}

#[test]
fn c1_exit_removes_its_untouched_socket() {
    // Contract 1 and 4 require both idle and stopped exits to remove their own unchanged socket.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    for idle in [Duration::ZERO, Duration::from_secs(60)] {
        let server = bind(&socket, idle);
        assert!(socket.exists());
        if !idle.is_zero() {
            server.stop_handle().stop();
        }

        server.run().unwrap();

        assert!(!socket.exists());
    }
}
