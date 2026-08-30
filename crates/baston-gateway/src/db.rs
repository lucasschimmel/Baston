//! Bridges the `baston-db` pool to the scripting `Db` surface.
//!
//! A newtype for the same reason [`crate::voice`] needs one: the trait lives in
//! `baston-scripting` and the pool in `baston-db`, and neither is local here.
//! It is also what keeps `baston-scripting` free of any SQL dependency — a
//! bundle built without the `db` capability compiles no database client at all.

use baston_db::{Db, QueryKind};
use baston_scripting::DbAccess;

/// The gateway's [`DbAccess`] implementation over the running pool.
pub struct GatewayDb(pub Db);

impl GatewayDb {
    fn kind(name: &str) -> Result<QueryKind, String> {
        QueryKind::parse(name).ok_or_else(|| {
            format!("unknown query kind \"{name}\" — expected rows, execute, scalar or insert")
        })
    }
}

impl DbAccess for GatewayDb {
    fn submit(
        &self,
        resource: &str,
        kind: &str,
        sql: String,
        params: Vec<serde_json::Value>,
    ) -> Result<u64, String> {
        Ok(self.0.submit(resource, Self::kind(kind)?, sql, params))
    }

    fn collect(&self, id: u64) -> Option<Result<serde_json::Value, String>> {
        self.0.collect(id)
    }

    fn query<'a>(
        &'a self,
        resource: &'a str,
        kind: &'a str,
        sql: String,
        params: Vec<serde_json::Value>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let kind = Self::kind(kind)?;
            self.0.run(resource, kind, &sql, &params).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_query_kind_lists_the_valid_ones() {
        let err = GatewayDb::kind("truncate").expect_err("not a query kind");
        for kind in ["rows", "execute", "scalar", "insert"] {
            assert!(err.contains(kind), "{err}");
        }
    }

    #[test]
    fn the_script_facing_names_map_to_their_kinds() {
        assert_eq!(GatewayDb::kind("rows"), Ok(QueryKind::Rows));
        assert_eq!(GatewayDb::kind("execute"), Ok(QueryKind::Execute));
        assert_eq!(GatewayDb::kind("scalar"), Ok(QueryKind::Scalar));
        assert_eq!(GatewayDb::kind("insert"), Ok(QueryKind::Insert));
    }
}
