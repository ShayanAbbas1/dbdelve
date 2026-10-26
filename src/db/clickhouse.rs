//! ClickHouse, over its HTTP interface. The driver is not written yet: the
//! engine is wired through the rest of the app and every call here answers
//! that it is not.

mod http;
mod tsv;

use super::{
    CancelToken, Catalog, Databases, DbError, Fetch, QueryResult, RelationKind, ServerConfig,
    Structure, plain_error,
};

/// The HTTP interface's plaintext port; its TLS one is 8443.
pub(super) const DEFAULT_PORT: u16 = 8123;

/// What a Cancel's error says, which is how `is_cancel` knows it.
pub(super) const CANCELLED: &str = "Cancelled.";

#[derive(Clone)]
pub struct Connection;

fn unwired<T>() -> Result<T, DbError> {
    Err(plain_error("ClickHouse is not wired yet.".into()))
}

impl Connection {
    pub fn open(_server: &ServerConfig) -> Result<Self, DbError> {
        unwired()
    }

    pub fn query(
        &self,
        _sql: &str,
        _cancel: &CancelToken,
        _fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        unwired()
    }

    pub fn generated(
        &self,
        _sql: &str,
        _cancel: &CancelToken,
        _fetch: Fetch,
    ) -> Result<QueryResult, DbError> {
        unwired()
    }

    pub fn query_alongside(
        &self,
        _sql: &str,
        _cancel: &CancelToken,
        _fetch: Fetch,
        _read_only: bool,
    ) -> Result<QueryResult, DbError> {
        unwired()
    }

    pub fn apart(&self) -> Self {
        self.clone()
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        unwired()
    }

    pub fn databases(&self) -> Result<Databases, DbError> {
        unwired()
    }

    pub fn structure(&self, _schema: &str, _relation: &str) -> Result<Structure, DbError> {
        unwired()
    }

    pub fn ddl(
        &self,
        _schema: &str,
        _relation: &str,
        _kind: RelationKind,
    ) -> Result<String, DbError> {
        unwired()
    }

    pub fn cancel(&self, _cancel: &CancelToken) -> Result<(), DbError> {
        unwired()
    }

    pub fn set_read_only(&self, _read_only: bool) -> Result<(), DbError> {
        unwired()
    }
}
