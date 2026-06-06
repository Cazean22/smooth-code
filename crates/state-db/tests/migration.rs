use std::sync::Arc;

use smooth_state_db::StateDbHandle;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn db_path(name: &str) -> Result<(TempDir, std::path::PathBuf), std::io::Error> {
    let root = tempfile::tempdir()?;
    let path = root
        .path()
        .join(format!("{name}/nested space/状态/state.db"));
    Ok((root, path))
}

#[tokio::test]
async fn migrations_are_idempotent_and_unicode_paths_work() -> TestResult {
    let (_root, path) = db_path("idempotent")?;
    let db = StateDbHandle::open(path.clone()).await?;
    db.upsert_thread("thread-1", None, None, None).await?;

    let reopened = StateDbHandle::open(path).await?;
    let row = reopened
        .get_thread("thread-1")
        .await?
        .ok_or_else(|| std::io::Error::other("row"))?;
    assert_eq!(row.thread_id, "thread-1");
    Ok(())
}

#[tokio::test]
async fn edge_round_trip_and_created_at_are_preserved() -> TestResult {
    let (_root, path) = db_path("edge")?;
    let db = StateDbHandle::open(path).await?;
    db.upsert_thread("parent", None, None, None).await?;
    db.upsert_thread(
        "child",
        Some("/root/child"),
        Some("child"),
        Some("default_subagent"),
    )
    .await?;

    db.upsert_open_edge("parent", "child").await?;

    let initial = db.list_open_children("parent").await?;
    assert_eq!(initial.len(), 1);
    let created_at = initial[0].created_at;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    db.upsert_open_edge("parent", "child").await?;
    let updated = db.list_open_children("parent").await?;
    assert_eq!(updated[0].created_at, created_at);

    db.close_edge("parent", "child").await?;
    assert!(db.list_open_children("parent").await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_edge_upserts_avoid_sqlite_busy() -> TestResult {
    let (_root, path) = db_path("concurrent")?;
    let db = Arc::new(StateDbHandle::open(path).await?);
    db.upsert_thread("parent", None, None, None).await?;

    let mut tasks = Vec::new();
    for idx in 0..8 {
        let db = Arc::clone(&db);
        tasks.push(tokio::spawn(async move {
            let child = format!("child-{idx}");
            db.upsert_thread(&child, Some(&format!("/root/{child}")), Some(&child), None)
                .await?;
            db.upsert_open_edge("parent", &child).await?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }));
    }
    for task in tasks {
        task.await??;
    }

    let rows = db.list_open_children("parent").await?;
    assert_eq!(rows.len(), 8);
    Ok(())
}
