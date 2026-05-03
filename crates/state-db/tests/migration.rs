use std::sync::Arc;

use smooth_state_db::StateDbHandle;
use tempfile::TempDir;

fn db_path(name: &str) -> (TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root
        .path()
        .join(format!("{name}/nested space/状态/state.db"));
    (root, path)
}

#[tokio::test]
async fn migrations_are_idempotent_and_unicode_paths_work() {
    let (_root, path) = db_path("idempotent");
    let db = StateDbHandle::open(path.clone()).await.expect("open db");
    db.upsert_thread("thread-1", None, None, None)
        .await
        .expect("upsert thread");

    let reopened = StateDbHandle::open(path).await.expect("reopen db");
    let row = reopened
        .get_thread("thread-1")
        .await
        .expect("get thread")
        .expect("row");
    assert_eq!(row.thread_id, "thread-1");
}

#[tokio::test]
async fn edge_round_trip_and_created_at_are_preserved() {
    let (_root, path) = db_path("edge");
    let db = StateDbHandle::open(path).await.expect("open db");
    db.upsert_thread("parent", None, None, None)
        .await
        .expect("upsert parent");
    db.upsert_thread("child", Some("/root/child"), Some("child"), Some("worker"))
        .await
        .expect("upsert child");
    db.upsert_open_edge("parent", "child")
        .await
        .expect("open edge");

    let initial = db
        .list_open_children("parent")
        .await
        .expect("list open children");
    assert_eq!(initial.len(), 1);
    let created_at = initial[0].created_at;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    db.upsert_open_edge("parent", "child")
        .await
        .expect("upsert edge again");
    let updated = db
        .list_open_children("parent")
        .await
        .expect("list open children");
    assert_eq!(updated[0].created_at, created_at);

    db.close_edge("parent", "child").await.expect("close edge");
    assert!(
        db.list_open_children("parent")
            .await
            .expect("list open children")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_edge_upserts_avoid_sqlite_busy() {
    let (_root, path) = db_path("concurrent");
    let db = Arc::new(StateDbHandle::open(path).await.expect("open db"));
    db.upsert_thread("parent", None, None, None)
        .await
        .expect("upsert parent");

    let mut tasks = Vec::new();
    for idx in 0..8 {
        let db = Arc::clone(&db);
        tasks.push(tokio::spawn(async move {
            let child = format!("child-{idx}");
            db.upsert_thread(&child, Some(&format!("/root/{child}")), Some(&child), None)
                .await
                .expect("upsert child");
            db.upsert_open_edge("parent", &child)
                .await
                .expect("upsert edge");
        }));
    }
    for task in tasks {
        task.await.expect("join");
    }

    let rows = db
        .list_open_children("parent")
        .await
        .expect("list open children");
    assert_eq!(rows.len(), 8);
}
