use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
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
