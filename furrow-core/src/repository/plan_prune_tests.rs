//! Plans a caller never seals are removed by later plans, without touching one
//! a caller may still seal.

use super::*;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        let data = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"first").unwrap();
        fs::write(root.join("b"), b"other").unwrap();
        Self {
            _temp: temp,
            root,
            data,
        }
    }

    fn attach(&self) -> FurrowRepository {
        FurrowRepository::attach_timeline_in(&self.data, &self.root, Some("chat"), None).unwrap()
    }
}

fn plans(repo: &FurrowRepository) -> PathBuf {
    repo.workspace_data_dir().join("capture-plans")
}

fn plan_file(repo: &FurrowRepository, token: &str) -> PathBuf {
    plans(repo).join(format!("{token}.json"))
}

/// A day is far past any time a caller takes between planning and sealing.
fn age_by_a_day(path: &Path) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let old = FileTime::from_unix_time(now.as_secs() as i64 - 24 * 60 * 60, 0);
    filetime::set_file_mtime(path, old).unwrap();
}

fn names(directory: &Path) -> BTreeSet<OsString> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect()
}

#[test]
fn a_new_plan_removes_abandoned_plans_and_keeps_a_fresh_one() {
    let f = Fixture::new();
    let mut repo = f.attach();
    repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap();
    let mut abandoned = Vec::new();
    for round in 0..40 {
        fs::write(f.root.join("a"), format!("round {round}")).unwrap();
        let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
        abandoned.push(plan_file(&repo, &plan.token));
    }
    // A writer that died between writing its plan and renaming it into place.
    let partial = plans(&repo).join(".tmpAbandoned");
    fs::write(&partial, b"{").unwrap();
    abandoned.push(partial);
    // Something the engine did not write is not its to remove.
    let foreign = plans(&repo).join("notes.txt");
    fs::write(&foreign, b"kept").unwrap();
    age_by_a_day(&foreign);
    for path in &abandoned {
        age_by_a_day(path);
    }
    fs::write(f.root.join("a"), b"settled").unwrap();
    let fresh = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();

    let newest = repo.capture_plan(Some(&[PathBuf::from("b")])).unwrap();

    for path in &abandoned {
        assert!(!path.exists(), "{} was not removed", path.display());
    }
    assert_eq!(
        names(&plans(&repo)),
        BTreeSet::from([
            OsString::from(format!("{}.json", fresh.token)),
            OsString::from(format!("{}.json", newest.token)),
            OsString::from("notes.txt"),
        ])
    );
    repo.seal_plan(None, SnapshotTrigger::AgentRun, &fresh.token)
        .unwrap();
}

#[test]
fn a_plan_removed_before_its_seal_asks_for_a_new_plan() {
    let f = Fixture::new();
    let mut repo = f.attach();
    repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap();
    let stalled = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
    age_by_a_day(&plan_file(&repo, &stalled.token));
    let current = repo.capture_plan(Some(&[PathBuf::from("b")])).unwrap();

    let error = repo
        .seal_plan(None, SnapshotTrigger::AgentRun, &stalled.token)
        .unwrap_err();

    assert!(
        format!("{error:#}").starts_with("capture plan is absent; re-plan"),
        "{error:#}"
    );
    repo.seal_plan(None, SnapshotTrigger::AgentRun, &current.token)
        .unwrap();
}

#[test]
fn sealing_while_another_handle_plans_and_prunes_keeps_every_fresh_plan() {
    let f = Fixture::new();
    let mut repo = f.attach();
    repo.seal(None, SnapshotTrigger::AgentRun, None).unwrap();
    let directory = plans(&repo);
    fs::create_dir_all(&directory).unwrap();
    let (root, data) = (f.root.clone(), f.data.clone());
    let planner = std::thread::spawn(move || {
        let other =
            FurrowRepository::attach_timeline_in(&data, &root, Some("chat"), None).unwrap();
        for round in 0..30 {
            let junk: Vec<PathBuf> = (0..20)
                .map(|n| {
                    let token = blake3::hash(format!("{round}/{n}").as_bytes()).to_hex();
                    let path = directory.join(format!("{token}.json"));
                    fs::write(&path, b"{}").unwrap();
                    age_by_a_day(&path);
                    path
                })
                .collect();
            other.capture_plan(Some(&[PathBuf::from("b")])).unwrap();
            for path in junk {
                assert!(!path.exists(), "round {round}: {} survived", path.display());
            }
        }
    });

    for round in 0..30 {
        fs::write(f.root.join("a"), format!("round {round}")).unwrap();
        let plan = repo.capture_plan(Some(&[PathBuf::from("a")])).unwrap();
        repo.seal_plan(None, SnapshotTrigger::AgentRun, &plan.token)
            .unwrap_or_else(|error| panic!("round {round}: {error:#}"));
    }
    planner.join().unwrap();
}
