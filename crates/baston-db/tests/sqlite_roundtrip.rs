//! End-to-end query behaviour against a real database.
//!
//! SQLite in memory, because it is the one driver that needs no server: the
//! job pipeline, the JSON conversion and the four query shapes are
//! driver-independent, so proving them here proves them everywhere.
#![cfg(feature = "sqlite")]

use std::time::Duration;

use baston_db::{Db, QueryKind};

async fn db() -> Db {
    let pool = baston_db::AnyPool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite"),
    );
    let db = Db::from_pool(pool, Duration::from_secs(5));
    await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Execute,
            "CREATE TABLE players (id INTEGER PRIMARY KEY, name TEXT, cash REAL, banned BOOLEAN)"
                .to_owned(),
            vec![],
        ),
    )
    .await
    .expect("schema");
    db
}

/// Collect a job, polling the way a script's runtime does.
async fn await_job(db: &Db, id: u64) -> Result<serde_json::Value, String> {
    for _ in 0..500 {
        if let Some(result) = db.collect(id) {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("job {id} never finished")
}

#[tokio::test]
async fn the_four_query_shapes_return_what_they_promise() {
    let db = db().await;

    let inserted = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Insert,
            "INSERT INTO players (name, cash, banned) VALUES (?, ?, ?)".to_owned(),
            vec![
                serde_json::json!("Lucas"),
                serde_json::json!(1250.5),
                serde_json::json!(false),
            ],
        ),
    )
    .await
    .unwrap();
    assert_eq!(inserted, serde_json::json!(1), "insert reports its new id");

    let rows = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Rows,
            "SELECT id, name, cash, banned FROM players".to_owned(),
            vec![],
        ),
    )
    .await
    .unwrap();
    let rows = rows.as_array().expect("rows come back as an array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Lucas");
    // An id must not come back as 1.0: resources use it as a key.
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[0]["cash"], 1250.5);

    let name = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Scalar,
            "SELECT name FROM players WHERE id = ?".to_owned(),
            vec![serde_json::json!(1)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(name, "Lucas");

    let affected = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Execute,
            "UPDATE players SET cash = ? WHERE id = ?".to_owned(),
            vec![serde_json::json!(0), serde_json::json!(1)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(affected, serde_json::json!(1));
}

#[tokio::test]
async fn parameters_are_bound_and_never_interpolated() {
    // The classic injection string must be stored as data, not executed.
    let db = db().await;
    let hostile = "Robert'); DROP TABLE players;--";
    await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Execute,
            "INSERT INTO players (name) VALUES (?)".to_owned(),
            vec![serde_json::json!(hostile)],
        ),
    )
    .await
    .unwrap();

    let stored = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Scalar,
            "SELECT name FROM players WHERE id = 1".to_owned(),
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        stored, hostile,
        "the table must still exist, with the string"
    );
}

#[tokio::test]
async fn a_null_column_reads_back_as_null() {
    let db = db().await;
    await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Execute,
            "INSERT INTO players (name, cash) VALUES (?, ?)".to_owned(),
            vec![serde_json::json!("Nobody"), serde_json::Value::Null],
        ),
    )
    .await
    .unwrap();
    let rows = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Rows,
            "SELECT cash FROM players".to_owned(),
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(rows[0]["cash"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_failing_query_reports_its_message_instead_of_panicking() {
    let db = db().await;
    let err = await_job(
        &db,
        db.submit(
            "test",
            QueryKind::Rows,
            "SELECT * FROM a_table_that_does_not_exist".to_owned(),
            vec![],
        ),
    )
    .await
    .expect_err("the table does not exist");
    assert!(err.contains("query failed"), "{err}");
}

#[tokio::test]
async fn a_result_is_collected_once() {
    let db = db().await;
    let id = db.submit("test", QueryKind::Scalar, "SELECT 1".to_owned(), vec![]);
    assert!(await_job(&db, id).await.is_ok());
    assert!(
        db.collect(id).is_none(),
        "a collected job must not replay its result"
    );
}

#[tokio::test]
async fn uncollected_results_do_not_accumulate_forever() {
    // A resource that submits a query then errors out before collecting would
    // otherwise leak one entry per query.
    let db = db().await;
    for _ in 0..5 {
        db.submit("test", QueryKind::Scalar, "SELECT 1".to_owned(), vec![]);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(db.pending_results(), 5);
    // Nothing is old enough to sweep yet — the retention window is a minute.
    assert_eq!(db.sweep(), 0);
    assert_eq!(db.pending_results(), 5);
}
