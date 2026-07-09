use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let data = temp.path().join("data");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "demo@example.com"]);
        git(&repo, &["config", "user.name", "Demo"]);
        fs::write(repo.join("app.txt"), b"tracked original\n").unwrap();
        fs::write(repo.join(".gitignore"), b".env\ncache/\n").unwrap();
        git(&repo, &["add", "app.txt", ".gitignore"]);
        git(&repo, &["commit", "-m", "initial"]);
        fs::write(repo.join(".env"), b"TOKEN=original\n").unwrap();
        fs::create_dir(repo.join("cache")).unwrap();
        fs::write(repo.join("cache/dependency.bin"), vec![7_u8; 180_000]).unwrap();
        fs::write(repo.join("notes.txt"), b"untracked notes\n").unwrap();
        symlink("app.txt", repo.join("app-link")).unwrap();
        let mut permissions = fs::metadata(repo.join("app.txt")).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(repo.join("app.txt"), permissions).unwrap();
        xattr::set(repo.join("app.txt"), "user.agit-test", b"preserved").unwrap();
        Self {
            _temp: temp,
            repo,
            data,
        }
    }

    fn agit(&self) -> Command {
        let mut command = Command::cargo_bin("agit").unwrap();
        command
            .env("AGIT_DATA_DIR", &self.data)
            .env("AGIT_NO_DAEMON", "1")
            .arg("--repo")
            .arg(&self.repo);
        command
    }

    fn watch(&self) -> String {
        let output = self
            .agit()
            .args(["--json", "watch"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let value: Value = serde_json::from_slice(&output).unwrap();
        value["snapshot"].as_str().unwrap().to_owned()
    }
}

#[test]
fn path_rewind_restores_ignored_secret_without_touching_new_work() {
    let fixture = Fixture::new();
    let snapshot = fixture.watch();
    fs::write(fixture.repo.join(".env"), b"TOKEN=destroyed\n").unwrap();
    fs::write(fixture.repo.join("later.txt"), b"keep this\n").unwrap();

    let preview = fixture
        .agit()
        .args(["rewind", &snapshot, "--paths", ".env", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&preview).unwrap();
    assert_eq!(value["changes"].as_array().unwrap().len(), 1);
    assert_eq!(value["changes"][0]["path"], ".env");
    assert_eq!(
        fs::read(fixture.repo.join(".env")).unwrap(),
        b"TOKEN=destroyed\n"
    );

    fixture
        .agit()
        .args(["rewind", &snapshot, "--paths", ".env", "--yes"])
        .assert()
        .success();
    assert_eq!(
        fs::read(fixture.repo.join(".env")).unwrap(),
        b"TOKEN=original\n"
    );
    assert_eq!(
        fs::read(fixture.repo.join("later.txt")).unwrap(),
        b"keep this\n"
    );
}

#[test]
fn full_rewind_restores_git_ignored_untracked_metadata_and_is_reversible() {
    let fixture = Fixture::new();
    let snapshot = fixture.watch();

    fs::remove_file(fixture.repo.join("app-link")).unwrap();
    fs::remove_file(fixture.repo.join("app.txt")).unwrap();
    fs::remove_file(fixture.repo.join(".env")).unwrap();
    fs::remove_dir_all(fixture.repo.join("cache")).unwrap();
    fs::write(fixture.repo.join("notes.txt"), b"destroyed notes\n").unwrap();

    let output = fixture
        .agit()
        .args(["rewind", &snapshot, "--yes"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let undo = output
        .lines()
        .find_map(|line| line.strip_prefix("Undo snapshot: "))
        .unwrap()
        .to_owned();

    assert_eq!(
        fs::read(fixture.repo.join("app.txt")).unwrap(),
        b"tracked original\n"
    );
    assert_eq!(
        fs::read(fixture.repo.join(".env")).unwrap(),
        b"TOKEN=original\n"
    );
    assert_eq!(
        fs::read(fixture.repo.join("notes.txt")).unwrap(),
        b"untracked notes\n"
    );
    assert_eq!(
        fs::read_link(fixture.repo.join("app-link")).unwrap(),
        Path::new("app.txt")
    );
    assert_eq!(
        fs::metadata(fixture.repo.join("app.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        xattr::get(fixture.repo.join("app.txt"), "user.agit-test")
            .unwrap()
            .unwrap(),
        b"preserved"
    );
    assert_eq!(
        fs::metadata(fixture.repo.join("cache/dependency.bin"))
            .unwrap()
            .len(),
        180_000
    );

    fixture
        .agit()
        .args(["rewind", &undo, "--yes"])
        .assert()
        .success();
    assert!(!fixture.repo.join("app.txt").exists());
    assert!(!fixture.repo.join(".env").exists());
    assert_eq!(
        fs::read(fixture.repo.join("notes.txt")).unwrap(),
        b"destroyed notes\n"
    );
}

#[test]
fn catalog_is_rebuilt_from_pack_and_authoritative_refs() {
    let fixture = Fixture::new();
    let snapshot = fixture.watch();
    let store = fixture.data.join("store-v1");
    for suffix in [
        "catalog.sqlite3",
        "catalog.sqlite3-wal",
        "catalog.sqlite3-shm",
    ] {
        let path = store.join(suffix);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }

    let output = fixture
        .agit()
        .args(["--json", "timeline"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let timeline: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(timeline[0]["id"], snapshot);
}

#[test]
fn incomplete_pack_tail_is_truncated_without_losing_visible_snapshots() {
    let fixture = Fixture::new();
    let snapshot = fixture.watch();
    let pack = fixture.data.join("store-v1/packs/pack-000001.agp");
    let valid_len = fs::metadata(&pack).unwrap().len();
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(&pack).unwrap();
    file.write_all(b"AGOB\x01").unwrap();
    file.sync_all().unwrap();

    fixture.agit().arg("status").assert().success();
    assert_eq!(fs::metadata(&pack).unwrap().len(), valid_len);
    let output = fixture
        .agit()
        .args(["--json", "timeline"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let timeline: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(timeline[0]["id"], snapshot);
}

#[test]
fn sqlite_consistent_rewind_restores_a_logical_database_snapshot() {
    let fixture = Fixture::new();
    let database = fixture.repo.join("dev.sqlite");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO items(value) VALUES('safe');",
            )
            .unwrap();
    }
    let snapshot = fixture.watch();
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("DROP TABLE items; CREATE TABLE damage(value TEXT);")
            .unwrap();
    }

    fixture
        .agit()
        .args([
            "rewind",
            &snapshot,
            "--paths",
            "dev.sqlite",
            "--sqlite-consistent",
            "--yes",
        ])
        .assert()
        .success();

    let connection = rusqlite::Connection::open(&database).unwrap();
    let value: String = connection
        .query_row("SELECT value FROM items WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "safe");
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

#[test]
fn path_rewind_refuses_to_follow_a_symlink_parent_outside_workspace() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.repo.join("nested")).unwrap();
    fs::write(fixture.repo.join("nested/value.txt"), b"inside\n").unwrap();
    let snapshot = fixture.watch();
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("value.txt"), b"outside\n").unwrap();
    fs::remove_dir_all(fixture.repo.join("nested")).unwrap();
    symlink(&outside, fixture.repo.join("nested")).unwrap();

    fixture
        .agit()
        .args(["rewind", &snapshot, "--paths", "nested/value.txt", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("symlink parent"));
    assert_eq!(fs::read(outside.join("value.txt")).unwrap(), b"outside\n");
}

#[test]
fn watch_refuses_non_git_directories() {
    let temp = tempfile::tempdir().unwrap();
    Command::cargo_bin("agit")
        .unwrap()
        .env("AGIT_DATA_DIR", temp.path().join("data"))
        .env("AGIT_NO_DAEMON", "1")
        .arg("--repo")
        .arg(temp.path())
        .arg("watch")
        .assert()
        .failure()
        .stderr(predicates::str::contains("requires a Git repository"));
}

#[test]
fn unchanged_snapshots_reuse_cached_blobs_and_add_only_small_metadata() {
    let fixture = Fixture::new();
    fixture.watch();
    let pack = fixture.data.join("store-v1/packs/pack-000001.agp");
    let initial = fs::metadata(&pack).unwrap().len();

    fixture
        .agit()
        .args(["snap", "-m", "unchanged"])
        .assert()
        .success();
    let unchanged_growth = fs::metadata(&pack).unwrap().len() - initial;
    assert!(
        unchanged_growth < 16 * 1024,
        "unchanged snapshot added {unchanged_growth} bytes"
    );

    fs::write(fixture.repo.join("notes.txt"), b"one small delta\n").unwrap();
    let before_delta = fs::metadata(&pack).unwrap().len();
    fixture
        .agit()
        .args(["snap", "-m", "small delta"])
        .assert()
        .success();
    let delta_growth = fs::metadata(&pack).unwrap().len() - before_delta;
    assert!(
        delta_growth < 32 * 1024,
        "small delta added {delta_growth} bytes"
    );
}

#[test]
fn warm_fork_is_independent_complete_listed_and_can_run_a_command() {
    let fixture = Fixture::new();
    fixture.watch();
    let destination = fixture._temp.path().join("agent-one");

    let output = fixture
        .agit()
        .args(["--json", "fork", "agent-one", "--destination"])
        .arg(&destination)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(summary["name"], "agent-one");
    assert!(
        summary["cloned_bytes"].as_u64().unwrap() + summary["copied_bytes"].as_u64().unwrap() > 0
    );
    assert_eq!(
        fs::read(destination.join(".env")).unwrap(),
        b"TOKEN=original\n"
    );
    assert_eq!(
        fs::read(destination.join("cache/dependency.bin"))
            .unwrap()
            .len(),
        180_000
    );
    assert_ne!(
        fs::read_to_string(destination.join(".agit/workspace-id")).unwrap(),
        fs::read_to_string(fixture.repo.join(".agit/workspace-id")).unwrap()
    );

    fs::write(destination.join("app.txt"), b"fork-only change\n").unwrap();
    assert_eq!(
        fs::read(fixture.repo.join("app.txt")).unwrap(),
        b"tracked original\n"
    );

    let listed = fixture
        .agit()
        .args(["--json", "forks"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: Value = serde_json::from_slice(&listed).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(
        listed[0]["destination"],
        destination.to_string_lossy().as_ref()
    );

    let command_destination = fixture._temp.path().join("agent-command");
    fixture
        .agit()
        .args(["fork", "agent-command", "--destination"])
        .arg(&command_destination)
        .args(["--", "sh", "-c", "printf isolated > command-result.txt"])
        .assert()
        .success();
    assert_eq!(
        fs::read(command_destination.join("command-result.txt")).unwrap(),
        b"isolated"
    );
    assert!(!fixture.repo.join("command-result.txt").exists());
}

#[test]
fn paged_flat_directory_snapshot_restores_entries_across_pages() {
    let fixture = Fixture::new();
    let flat = fixture.repo.join("flat");
    fs::create_dir(&flat).unwrap();
    for index in 0..1_200 {
        fs::write(
            flat.join(format!("entry-{index:04}.txt")),
            format!("value {index}\n"),
        )
        .unwrap();
    }
    let snapshot = fixture.watch();
    for index in [0, 599, 1_199] {
        fs::remove_file(flat.join(format!("entry-{index:04}.txt"))).unwrap();
    }

    fixture
        .agit()
        .args(["rewind", &snapshot, "--paths", "flat", "--yes"])
        .assert()
        .success();
    for index in [0, 599, 1_199] {
        assert_eq!(
            fs::read(flat.join(format!("entry-{index:04}.txt"))).unwrap(),
            format!("value {index}\n").as_bytes()
        );
    }
}

#[test]
fn interrupted_rewind_rolls_back_to_pre_rewind_state_on_next_command() {
    let fixture = Fixture::new();
    let snapshot = fixture.watch();
    fs::write(fixture.repo.join(".env"), b"TOKEN=broken\n").unwrap();
    fs::write(fixture.repo.join("app.txt"), b"broken app\n").unwrap();

    fixture
        .agit()
        .env("AGIT_FAILPOINT", "rewind_after_first_change")
        .args(["rewind", &snapshot, "--yes"])
        .assert()
        .code(86);

    // Opening the repository detects the durable intent and restores the
    // automatic pre-rewind snapshot before serving status.
    fixture
        .agit()
        .arg("status")
        .assert()
        .success()
        .stderr(predicates::str::contains("recovering interrupted rewind"));
    assert_eq!(
        fs::read(fixture.repo.join(".env")).unwrap(),
        b"TOKEN=broken\n"
    );
    assert_eq!(
        fs::read(fixture.repo.join("app.txt")).unwrap(),
        b"broken app\n"
    );
}

#[test]
fn foreground_watcher_seals_after_write_quiescence() {
    let fixture = Fixture::new();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agit"))
        .env("AGIT_DATA_DIR", &fixture.data)
        .arg("--repo")
        .arg(&fixture.repo)
        .args(["watch", "--foreground", "--debounce-ms", "100"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.repo.join(".agit/workspace-id").exists() {
        assert!(Instant::now() < deadline, "watcher did not attach in time");
        std::thread::sleep(Duration::from_millis(25));
    }
    // The initial snapshot precedes watcher installation. Give the native
    // backend time to enter its event loop before creating the test event.
    // Avoid polling the growing pack during this one-time ingest.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "foreground watcher exited during initial protection"
    );
    fs::write(
        fixture.repo.join("automatic.txt"),
        b"captured automatically\n",
    )
    .unwrap();

    let mut saw_watcher_snapshot = false;
    while Instant::now() < deadline {
        let output = fixture
            .agit()
            .args(["--json", "timeline"])
            .output()
            .unwrap();
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            saw_watcher_snapshot = value
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["trigger"] == "watcher");
            if saw_watcher_snapshot {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    child.kill().ok();
    child.wait().ok();
    assert!(saw_watcher_snapshot, "watcher did not publish a snapshot");
}

#[test]
fn default_watch_starts_background_protection_and_forget_stops_it() {
    let fixture = Fixture::new();
    std::process::Command::new(env!("CARGO_BIN_EXE_agit"))
        .env("AGIT_DATA_DIR", &fixture.data)
        .arg("--repo")
        .arg(&fixture.repo)
        .arg("watch")
        .status()
        .unwrap()
        .success()
        .then_some(())
        .expect("watch failed");

    let output = fixture.agit().args(["--json", "status"]).output().unwrap();
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["watcher_running"], true);

    fixture.agit().arg("forget").assert().success();
    let workspace = fixture.data.join("store-v1/workspaces");
    let pid_files: Vec<_> = fs::read_dir(workspace)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("daemon.pid"))
        .filter(|path| path.exists())
        .collect();
    assert!(pid_files.is_empty());
}

#[test]
fn recovery_rediscovers_workspace_after_git_clean_removes_pointer() {
    let fixture = Fixture::new();
    let snapshot = fixture.watch();
    git(&fixture.repo, &["clean", "-fdx"]);
    assert!(!fixture.repo.join(".agit/workspace-id").exists());
    assert!(!fixture.repo.join(".env").exists());

    fixture
        .agit()
        .args(["rewind", &snapshot, "--yes"])
        .assert()
        .success();
    assert_eq!(
        fs::read(fixture.repo.join(".env")).unwrap(),
        b"TOKEN=original\n"
    );
    assert!(fixture.repo.join(".agit/workspace-id").exists());
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}
