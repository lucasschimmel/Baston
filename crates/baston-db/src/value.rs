//! JSON ↔ SQL conversion.
//!
//! JSON is the boundary format everywhere else in BASTON's scripting layer, so
//! it is the boundary format here too: a script sends parameters as JSON and
//! reads rows back as JSON, whichever engine it runs on and whichever driver
//! answers.

use crate::{DbError, QueryKind};

/// Decode one row into a JSON object keyed by column name.
///
/// Values are read through the driver's own JSON/text decoders where possible
/// and fall back to a string, because a game resource reading an exotic column
/// type wants *something* it can print, not a failed query.
pub(crate) fn row_to_json<R>(row: &R) -> Result<serde_json::Value, DbError>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    for<'a> Option<serde_json::Value>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<String>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<i64>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<f64>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<bool>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    use sqlx::Column as _;

    let mut object = serde_json::Map::with_capacity(row.columns().len());
    for column in row.columns() {
        let name = column.name();
        object.insert(name.to_owned(), column_to_json(row, name));
    }
    Ok(serde_json::Value::Object(object))
}

/// The first column of a row, for `scalar` queries.
pub(crate) fn first_column_to_json<R>(row: &R) -> Result<serde_json::Value, DbError>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    for<'a> Option<serde_json::Value>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<String>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<i64>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<f64>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<bool>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    use sqlx::Column as _;

    match row.columns().first() {
        Some(column) => Ok(column_to_json(row, column.name())),
        None => Ok(serde_json::Value::Null),
    }
}

/// Read one column, trying the types a game resource actually stores.
///
/// Order matters: integers before floats so an id does not come back as
/// `1.0`, and the string fallback last so nothing decodes to it by accident.
fn column_to_json<R>(row: &R, name: &str) -> serde_json::Value
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    for<'a> Option<serde_json::Value>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<String>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<i64>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<f64>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<bool>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    if let Ok(value) = row.try_get::<Option<i64>, _>(name) {
        return value.map_or(serde_json::Value::Null, serde_json::Value::from);
    }
    if let Ok(value) = row.try_get::<Option<f64>, _>(name) {
        return value.map_or(serde_json::Value::Null, serde_json::Value::from);
    }
    if let Ok(value) = row.try_get::<Option<bool>, _>(name) {
        return value.map_or(serde_json::Value::Null, serde_json::Value::from);
    }
    if let Ok(value) = row.try_get::<Option<serde_json::Value>, _>(name) {
        return value.unwrap_or(serde_json::Value::Null);
    }
    if let Ok(value) = row.try_get::<Option<String>, _>(name) {
        return value.map_or(serde_json::Value::Null, serde_json::Value::from);
    }
    // An exotic column type: a resource wants something it can print, not a
    // failed query over a column it may not even read.
    serde_json::Value::Null
}

/// Shape the answer to an `execute`/`insert`.
pub(crate) fn describe_execute<D>(kind: QueryKind, done: &D) -> serde_json::Value
where
    D: ExecuteInfo,
{
    match kind {
        QueryKind::Insert => done
            .last_insert_id()
            .map_or(serde_json::Value::Null, serde_json::Value::from),
        _ => serde_json::Value::from(done.rows_affected()),
    }
}

/// What an execute result can tell us, across drivers.
///
/// Postgres has no `last_insert_id`: an insert there reports its id through
/// `RETURNING`, so the trait says `None` rather than inventing a zero that a
/// resource would store as a real key.
pub(crate) trait ExecuteInfo {
    fn rows_affected(&self) -> u64;
    fn last_insert_id(&self) -> Option<i64>;
}

#[cfg(feature = "sqlite")]
impl ExecuteInfo for sqlx::sqlite::SqliteQueryResult {
    fn rows_affected(&self) -> u64 {
        Self::rows_affected(self)
    }
    fn last_insert_id(&self) -> Option<i64> {
        Some(self.last_insert_rowid())
    }
}

#[cfg(feature = "mysql")]
impl ExecuteInfo for sqlx::mysql::MySqlQueryResult {
    fn rows_affected(&self) -> u64 {
        Self::rows_affected(self)
    }
    fn last_insert_id(&self) -> Option<i64> {
        Some(self.last_insert_id() as i64)
    }
}

#[cfg(feature = "postgres")]
impl ExecuteInfo for sqlx::postgres::PgQueryResult {
    fn rows_affected(&self) -> u64 {
        Self::rows_affected(self)
    }
    fn last_insert_id(&self) -> Option<i64> {
        // Postgres reports generated ids through RETURNING, not out of band.
        None
    }
}

/// Bind one JSON parameter, per driver.
///
/// Three near-identical functions rather than a generic one: the `Encode`
/// bounds differ per backend, and spelling them out generically costs more
/// than it saves for three call sites.
macro_rules! bind_impl {
    ($name:ident, $db:ty) => {
        pub(crate) fn $name<'q>(
            query: sqlx::query::Query<'q, $db, <$db as sqlx::Database>::Arguments<'q>>,
            param: &'q serde_json::Value,
        ) -> sqlx::query::Query<'q, $db, <$db as sqlx::Database>::Arguments<'q>> {
            match param {
                serde_json::Value::Null => query.bind(None::<String>),
                serde_json::Value::Bool(b) => query.bind(*b),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        query.bind(i)
                    } else {
                        query.bind(n.as_f64().unwrap_or_default())
                    }
                }
                serde_json::Value::String(s) => query.bind(s.as_str()),
                // Arrays and objects go over as their JSON text: a driver-side
                // JSON type would be wrong for the many schemas that store
                // them in a plain TEXT column.
                other => query.bind(other.to_string()),
            }
        }
    };
}

#[cfg(feature = "sqlite")]
bind_impl!(bind_sqlite, sqlx::Sqlite);
#[cfg(feature = "postgres")]
bind_impl!(bind_postgres, sqlx::Postgres);
#[cfg(feature = "mysql")]
bind_impl!(bind_mysql, sqlx::MySql);
