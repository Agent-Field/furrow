//! Asking about a folder the store has never watched.

use super::*;

#[test]
fn an_unwatched_folder_is_answered_without_opening_the_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let folder = temp.path().join("work");
    fs::create_dir_all(&folder).unwrap();

    let before = crate::catalog::work_steps();
    let error = FurrowRepository::open_timeline_in(&data, &folder, None, None)
        .err()
        .expect("an unwatched folder does not open");

    assert!(
        error.to_string().contains("not watched"),
        "unexpected error: {error:#}"
    );
    assert_eq!(crate::catalog::work_steps(), before);
    assert!(
        !data.join(STORE_DIR).exists(),
        "the question created a store"
    );
}

#[test]
fn an_unwatched_folder_is_not_watched_in_a_populated_store_either() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let watched = temp.path().join("watched");
    let other = temp.path().join("other");
    fs::create_dir_all(&watched).unwrap();
    fs::create_dir_all(&other).unwrap();
    fs::write(watched.join("a.txt"), "a").unwrap();
    FurrowRepository::attach_and_seal_timeline_in(
        &data,
        &watched,
        None,
        None,
        SnapshotTrigger::AgentRun,
        SealOptions::default(),
    )
    .unwrap();

    let before = crate::catalog::work_steps();
    let error = FurrowRepository::open_timeline_in(&data, &other, None, None)
        .err()
        .expect("an unwatched folder does not open");
    assert!(error.to_string().contains("not watched"));
    assert_eq!(crate::catalog::work_steps(), before);
    // The watched folder still opens.
    FurrowRepository::open_timeline_in(&data, &watched, None, None).unwrap();
}

#[test]
fn a_watched_folder_opens_while_another_process_holds_the_catalog_write_lock() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let watched = temp.path().join("watched");
    fs::create_dir_all(&watched).unwrap();
    fs::write(watched.join("a.txt"), "a").unwrap();
    FurrowRepository::attach_and_seal_timeline_in(
        &data,
        &watched,
        None,
        None,
        SnapshotTrigger::AgentRun,
        SealOptions::default(),
    )
    .unwrap();
    let writer = rusqlite::Connection::open(data.join(STORE_DIR).join("catalog.sqlite3")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let (done, opened) = std::sync::mpsc::channel();
    let reader = std::thread::spawn({
        let watched = watched.clone();
        move || {
            let repository = FurrowRepository::open_timeline_in(&data, &watched, None, None)
                .and_then(|repository| repository.status().map(|_| ()));
            done.send(repository.is_ok()).unwrap();
        }
    });
    let finished = opened.recv_timeout(std::time::Duration::from_secs(3));
    writer.execute_batch("ROLLBACK").unwrap();
    reader.join().unwrap();
    assert_eq!(
        finished,
        Ok(true),
        "status queued behind the catalog write lock"
    );
}
