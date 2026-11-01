//! An engine plugin: a separate executable DBDelve starts for each connection
//! made through it, speaking `dbdelve-plugin-protocol` over its stdin and
//! stdout.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub struct Plugin {
    /// `[a-z0-9-]+`, persisted as `plugin:<id>`.
    pub id: String,
    /// What errors and the connection form call it: "The ODBC plugin stopped".
    pub name: String,
    pub version: String,
    pub executable: PathBuf,
}
