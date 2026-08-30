//! The driver-agnostic pool.
//!
//! One enum rather than a trait object: there are exactly three drivers, they
//! are chosen at build time, and sqlx's types differ enough per backend that a
//! trait would be more indirection than the three arms it replaces.

use std::fmt;

use crate::{DbError, QueryKind};

/// Which backend a pool speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Sqlite,
    Postgres,
    MySql,
}

impl Driver {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
        }
    }

    /// The URL schemes that select this driver.
    const fn schemes(self) -> &'static [&'static str] {
        match self {
            Self::Sqlite => &["sqlite:"],
            Self::Postgres => &["postgres://", "postgresql://"],
            Self::MySql => &["mysql://", "mariadb://"],
        }
    }

    pub const fn is_compiled_in(self) -> bool {
        match self {
            Self::Sqlite => cfg!(feature = "sqlite"),
            Self::Postgres => cfg!(feature = "postgres"),
            Self::MySql => cfg!(feature = "mysql"),
        }
    }

    /// The driver a connection URL asks for.
    pub fn of_url(url: &str) -> Option<Self> {
        let url = url.trim();
        [Self::Sqlite, Self::Postgres, Self::MySql]
            .into_iter()
            .find(|driver| driver.schemes().iter().any(|s| url.starts_with(s)))
    }
}

impl fmt::Display for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A connection pool for whichever backend was configured.
///
/// `Debug` prints the driver only: a pool's inner options carry the connection
/// URL, and that carries a password.
#[derive(Clone)]
pub enum AnyPool {
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "mysql")]
    MySql(sqlx::MySqlPool),
}

impl fmt::Debug for AnyPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AnyPool").field(&self.driver()).finish()
    }
}

impl AnyPool {
    pub fn driver(&self) -> Driver {
        match self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => Driver::Sqlite,
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => Driver::Postgres,
            #[cfg(feature = "mysql")]
            Self::MySql(_) => Driver::MySql,
        }
    }

    /// Run one query and shape the answer according to `kind`.
    pub async fn run(
        &self,
        kind: QueryKind,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<serde_json::Value, DbError> {
        match self {
            #[cfg(feature = "sqlite")]
            Self::Sqlite(pool) => sqlite::run(pool, kind, sql, params).await,
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => postgres::run(pool, kind, sql, params).await,
            #[cfg(feature = "mysql")]
            Self::MySql(pool) => mysql::run(pool, kind, sql, params).await,
        }
    }
}

/// Open a pool for `url`.
///
/// Refuses rather than guessing when the URL names a driver this build does
/// not contain — the alternative is a connection error that never mentions the
/// real cause.
pub async fn connect(url: &str, pool_size: u32) -> Result<AnyPool, DbError> {
    let driver = Driver::of_url(url).ok_or(DbError::UnknownUrlScheme)?;
    if !driver.is_compiled_in() {
        return Err(DbError::DriverAbsent(driver.label().to_owned()));
    }
    let size = pool_size.max(1);
    match driver {
        #[cfg(feature = "sqlite")]
        Driver::Sqlite => {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(size)
                .connect(url)
                .await
                .map_err(|e| DbError::Connect(e.to_string()))?;
            Ok(AnyPool::Sqlite(pool))
        }
        #[cfg(feature = "postgres")]
        Driver::Postgres => {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(size)
                .connect(url)
                .await
                .map_err(|e| DbError::Connect(e.to_string()))?;
            Ok(AnyPool::Postgres(pool))
        }
        #[cfg(feature = "mysql")]
        Driver::MySql => {
            let pool = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(size)
                .connect(url)
                .await
                .map_err(|e| DbError::Connect(e.to_string()))?;
            Ok(AnyPool::MySql(pool))
        }
        // Unreachable: the compiled-in check above already refused.
        #[allow(unreachable_patterns)]
        other => Err(DbError::DriverAbsent(other.label().to_owned())),
    }
}

/// The per-driver query bodies.
///
/// They differ only in the sqlx types involved, so each is a short module
/// rather than a layer of generics that would obscure three concrete cases.
macro_rules! driver_impl {
    ($name:ident, $row:ty, $args:ty, $bind:path) => {
        mod $name {
            use super::*;

            pub async fn run(
                pool: &sqlx::Pool<<$row as sqlx::Row>::Database>,
                kind: QueryKind,
                sql: &str,
                params: &[serde_json::Value],
            ) -> Result<serde_json::Value, DbError> {
                let mut query = sqlx::query::<<$row as sqlx::Row>::Database>(sql);
                for param in params {
                    query = $bind(query, param);
                }
                match kind {
                    QueryKind::Rows => {
                        let rows = query
                            .fetch_all(pool)
                            .await
                            .map_err(|e| DbError::Query(e.to_string()))?;
                        let mut out = Vec::with_capacity(rows.len());
                        for row in &rows {
                            out.push(crate::value::row_to_json(row)?);
                        }
                        Ok(serde_json::Value::Array(out))
                    }
                    QueryKind::Scalar => {
                        let row = query
                            .fetch_optional(pool)
                            .await
                            .map_err(|e| DbError::Query(e.to_string()))?;
                        match row {
                            Some(row) => crate::value::first_column_to_json(&row),
                            None => Ok(serde_json::Value::Null),
                        }
                    }
                    QueryKind::Execute | QueryKind::Insert => {
                        let done = query
                            .execute(pool)
                            .await
                            .map_err(|e| DbError::Query(e.to_string()))?;
                        Ok(crate::value::describe_execute(kind, &done))
                    }
                }
            }
        }
    };
}

#[cfg(feature = "sqlite")]
driver_impl!(
    sqlite,
    sqlx::sqlite::SqliteRow,
    sqlx::sqlite::SqliteArguments,
    crate::value::bind_sqlite
);
#[cfg(feature = "postgres")]
driver_impl!(
    postgres,
    sqlx::postgres::PgRow,
    sqlx::postgres::PgArguments,
    crate::value::bind_postgres
);
#[cfg(feature = "mysql")]
driver_impl!(
    mysql,
    sqlx::mysql::MySqlRow,
    sqlx::mysql::MySqlArguments,
    crate::value::bind_mysql
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_select_their_driver() {
        assert_eq!(Driver::of_url("sqlite:baston.db"), Some(Driver::Sqlite));
        assert_eq!(
            Driver::of_url("postgres://user@host/db"),
            Some(Driver::Postgres)
        );
        assert_eq!(
            Driver::of_url("postgresql://user@host/db"),
            Some(Driver::Postgres)
        );
        assert_eq!(Driver::of_url("mysql://user@host/db"), Some(Driver::MySql));
        // MariaDB is the same wire protocol, and it is what most FiveM servers
        // actually run.
        assert_eq!(
            Driver::of_url("mariadb://user@host/db"),
            Some(Driver::MySql)
        );
        assert_eq!(Driver::of_url("mongodb://host"), None);
        assert_eq!(Driver::of_url("baston.db"), None);
    }

    #[tokio::test]
    async fn an_absent_driver_is_named_rather_than_failing_to_connect() {
        // Compiled without postgres, this must say so instead of surfacing a
        // connection error that never mentions the real cause.
        if Driver::Postgres.is_compiled_in() {
            return;
        }
        let err = connect("postgres://user@host/db", 4)
            .await
            .expect_err("no postgres driver here");
        assert!(matches!(err, DbError::DriverAbsent(ref d) if d == "postgres"));
    }

    #[tokio::test]
    async fn an_unsupported_scheme_is_refused_before_connecting() {
        let err = connect("mongodb://host/db", 4)
            .await
            .expect_err("not a SQL url");
        assert!(matches!(err, DbError::UnknownUrlScheme));
    }
}
