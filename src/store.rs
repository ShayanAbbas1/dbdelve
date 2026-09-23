use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::{Cell, RelationKind};

const PROFILES_FILE: &str = "profiles.toml";
/// The release variant's name. Both the support directory and the keychain
/// service are this, so [`variant_name`] is the only thing that may widen it.
const NAME: &str = "dbdelve";
/// The buffer file a build before per-tab buffers wrote. Read-only now, and
/// only for tab 0 -- see [`read_scratch`].
const LEGACY_SCRATCH_FILE: &str = ".scratch.sql";
const HISTORY_FILE: &str = ".history.jsonl";
/// How far back the history reads, and what [`compact_history`] leaves on
/// disk once the slack is used up.
pub const HISTORY_DEPTH: usize = 200;
/// How far past [`HISTORY_DEPTH`] the file may run before it is rewritten.
///
/// Rewriting on every run would cost a full write per statement to keep a
/// handful of lines off the end. Letting it drift and compacting in one go
/// amortises that to one rewrite per [`HISTORY_SLACK`] minus
/// [`HISTORY_DEPTH`] runs, and bounds the file either way -- which is what
/// makes reading the whole of it cheap.
const HISTORY_SLACK: usize = HISTORY_DEPTH * 4;
/// A grid past this many rows still runs and displays in full -- this is only
/// how much of it a snapshot keeps on disk, so reopening a tab is instant
/// without the cache growing as large as the result it is caching.
pub const GRID_ROW_CAP: usize = 5_000;

/// Field order is load-bearing: TOML cannot emit a scalar after a table, so
/// every scalar has to precede the `open_queries` and `open_objects` arrays.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    /// libpq's spelling, so a profile file stays readable and a mode dbdelve
    /// stops supporting reads back as a name rather than a number. Defaulted,
    /// so profiles written before TLS existed load as `prefer` — which is what
    /// they were connecting as.
    #[serde(default)]
    pub sslmode: Option<String>,
    #[serde(default)]
    pub root_certificate: Option<String>,
    /// `db::Engine::as_str()`. Absent is a profile written before a second
    /// engine existed, and reads back as Postgres -- which is what it was
    /// connecting as.
    #[serde(default)]
    pub engine: Option<String>,
    /// The database file, for SQLite. Absent for a server engine.
    #[serde(default)]
    pub path: Option<String>,
    /// What Snowflake needs beyond the fields it shares with a server engine.
    /// All four absent for every other engine. `private_key` is a path: the
    /// key itself never leaves its file.
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub warehouse: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    /// The editor's zoom, back when it was a per-profile setting. Read only:
    /// the live value is [`StoredSettings::editor_font_size`] now, and this is
    /// what the migration seeds it from for anyone upgrading -- dropping the
    /// field would take their zoom with it on the next save.
    #[serde(default)]
    pub editor_font_size: Option<f32>,
    /// Seconds a statement may run before the engine stops it, or 0 / absent
    /// for no limit. Absent is a profile written before the field existed, and
    /// no limit is exactly what it was running with.
    #[serde(default)]
    pub statement_timeout: Option<u32>,
    /// The id the next query buffer will be given. Persisted rather than
    /// derived from the open buffers, because deriving it hands a closed tab's
    /// id back out -- and a new buffer with a dead tab's id reads that tab's
    /// snapshot as its own. Absent is a profile written before it was kept, and
    /// the loader falls back to the derived value for one.
    #[serde(default)]
    pub next_query_id: Option<u64>,
    /// `theme::ConnectionColor::slug()`. Absent is a profile written before a
    /// connection could carry one, and reads back as no colour -- which is
    /// exactly how it has always been drawn.
    #[serde(default)]
    pub color: Option<String>,
    /// `sql::Mode::slug()`. Absent is a profile written before modes existed,
    /// and reads back as Read-write -- which is what it has always been
    /// connecting as. Stored as the raw slug and decoded in `restore_profile`
    /// for the same reason `color` is: a value written by a build this one is
    /// older than must cost the user that one field, not every connection in
    /// the file.
    #[serde(default)]
    pub mode: Option<String>,
    /// `sql::Destructive::slug()`, for the kinds whose confirmation this
    /// connection has silenced. Per connection and per kind: silencing DROP on
    /// a scratch database says nothing about whether you want to be asked on
    /// prod. Raw slugs, and an entry this build cannot read is dropped -- that
    /// kind simply keeps asking, which is the safe direction.
    #[serde(default)]
    pub confirmed: Vec<String>,
    /// The name of the one query buffer a profile had, before a profile could
    /// have several. Read only: nothing writes it any more, and it is kept
    /// because every profile on disk today carries its open query here and
    /// removing the field would drop it on the next save.
    #[serde(default)]
    pub open_query: Option<String>,
    /// The query buffers this profile had open, in strip order.
    #[serde(default)]
    pub open_queries: Vec<StoredQueryTab>,
    #[serde(default)]
    pub open_objects: Vec<StoredObject>,
}

/// One query buffer in the strip. The SQL itself is not here: a saved buffer
/// lives in its query file and an unsaved one in its scratch file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredQueryTab {
    pub id: u64,
    /// The saved query in this buffer, or absent for an unsaved one.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub active: bool,
}

/// An opened table, view or routine, stored by name rather than by content: the
/// catalog is the source of truth for what it holds, so a restored tab shows
/// today's definition and one that has been dropped simply does not come back.
///
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredObject {
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub routine: bool,
    /// A relation's kind, which the catalog would otherwise be the only source
    /// of -- and waiting for it is what kept a restored relation out of the tab
    /// strip. Meaningless for a routine.
    #[serde(default)]
    pub kind: RelationKind,
    /// The `WHERE` expression this tab narrows the relation by, without the
    /// keyword. Part of the tab's identity, not a setting on it: a tab that
    /// forgot its filter would come back from a restart as a different tab and
    /// collide with its sibling. Absent is a profile written before filters
    /// existed, and reads back as the whole relation -- which is what that tab
    /// had open. Meaningless for a routine.
    #[serde(default)]
    pub filter: String,
    /// The column-and-value bars a build before operators wrote. Read only:
    /// [`Self::bars`] supersedes it and is written in its place, and a pair
    /// read back here is the equality joined by `AND` that it was.
    #[serde(default)]
    pub filters: Vec<(String, String)>,
    /// Which tab was in front. A flag on the object rather than a pointer to
    /// it: a name can contain anything, including whatever would separate a
    /// schema from a relation in a key.
    #[serde(default)]
    pub active: bool,
    /// The filter bars [`Self::filter`] was derived from. The bars are the
    /// editable state and the expression is what runs, so both are kept:
    /// nothing here parses a `WHERE` back into controls. Last, because TOML
    /// cannot emit a scalar after a table and every bar is one. Meaningless for
    /// a routine.
    #[serde(default)]
    pub bars: Vec<StoredFilter>,
}

/// One filter bar on disk (spec §2.4). Every field defaults, so a bar written
/// by a build that knew fewer of them still loads -- and an operator or joiner
/// this build cannot read comes back as the equality joined by `AND` that every
/// bar was before they existed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StoredFilter {
    #[serde(default)]
    pub column: String,
    #[serde(default)]
    pub value: String,
    /// `main::Operator::slug`. A name rather than an index, so inserting an
    /// operator cannot silently rewrite what everyone's saved bars mean.
    #[serde(default)]
    pub operator: String,
    /// `AND` or `OR`: how this bar joins to the one above it.
    #[serde(default)]
    pub conjunction: String,
    /// Whether the value is the user's own SQL rather than a column and an
    /// operator.
    #[serde(default)]
    pub raw: bool,
}

/// A tab's last-seen grid, kept so reopening a profile shows a query's or a
/// table's data immediately rather than an empty grid until it reruns. A
/// cache, not a source of truth -- [`read_grid`] hands back `None` rather than
/// an error for anything it cannot make sense of, and nothing here is ever
/// written back to the database it came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGrid {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// The row count before [`GRID_ROW_CAP`] trimmed it, so the UI can say
    /// "5000 of N" for a result that did not fit whole.
    pub total_rows: usize,
    #[serde(default)]
    pub sort: Vec<(usize, bool)>,
    /// A relation tab's `ORDER BY`, as expression and direction. [`Self::sort`]
    /// is what the headers draw; this is what rebuilds the statement, and
    /// without it a refresh asks for no order at all and returns rows in a
    /// different order than the snapshot it replaces.
    #[serde(default)]
    pub order_by: Vec<(String, bool)>,
    #[serde(default)]
    pub widths: Vec<f32>,
    #[serde(default)]
    pub active: Option<(usize, usize)>,
    #[serde(default)]
    pub last_query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// The relation tab's filter, so reopening dbdelve lands on the rows that
    /// were being read rather than on the whole table. Kept for the same reason
    /// [`Self::order_by`] is, and unlike the page offset, which is not part of
    /// what a tab is showing.
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub showing_structure: bool,
    #[serde(default)]
    pub captured: u64,
}

/// The three font families in use. App-level rather than per-profile: the face
/// dbdelve is read in belongs to the person reading, not to the database they
/// happen to be connected to. Every field is optional, so a file written before
/// fonts were pickable reads back as the defaults -- which is what it was drawn
/// in.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredFonts {
    #[serde(default)]
    pub chrome: Option<String>,
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default)]
    pub grid: Option<String>,
}

/// What the app is set to, as opposed to what a connection is. App-level for
/// the same reason the fonts are: the theme, the zoom and how many rows a
/// preview asks for belong to the person reading, not to the database they
/// happen to be connected to. Every field is optional, so a file written
/// before settings existed reads back as the defaults -- which is what it was
/// running with.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredSettings {
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub editor_font_size: Option<f32>,
    #[serde(default)]
    pub preview_rows: Option<usize>,
    #[serde(default)]
    pub opacity: Option<f32>,
    /// Keybinding overrides, keyed by the action id in
    /// `keybindings::REGISTRY`. Only the ones a user actually changed --
    /// everything else stays on whatever the running build defaults to.
    #[serde(default)]
    pub custom_keybindings: Option<HashMap<String, String>>,
}

/// A decoded profile file: the profiles, the id of the one that was in front,
/// the fonts and the settings. Named because it is four things and a bare
/// tuple in the signature reads as none of them.
type Restored = (
    Vec<StoredProfile>,
    Option<String>,
    Option<StoredFonts>,
    Option<StoredSettings>,
);

/// Field order is load-bearing here too: `active` is a scalar, so it has to
/// precede every table, and `fonts` and `settings` are tables, so both have to
/// precede the profile table array.
#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
struct ProfileFile {
    /// The profile that was in front. An id rather than a flag on the profile,
    /// unlike [`StoredObject::active`]: an id is a validated slug, so there is
    /// nothing in one that could be mistaken for a key.
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    fonts: Option<StoredFonts>,
    #[serde(default)]
    settings: Option<StoredSettings>,
    #[serde(default)]
    profiles: Vec<StoredProfile>,
}

/// The profile list and the id of the one that was last in front. A missing
/// file is the first run, and reads as an empty list. Every other failure is
/// reported, a missing `HOME` included -- `save_profiles` refuses on that too,
/// and an empty list here is what the next save writes back.
pub fn load_profiles() -> Result<Restored, String> {
    let path = dbdelve_directory()?.join(PROFILES_FILE);
    // A file we could not read is not renamed: nothing is recovered by moving
    // it, so the overwrite hazard below technically remains. A directory we
    // cannot read is one we almost certainly cannot write either.
    let Some(text) = read_file(&path)? else {
        return Ok((Vec::new(), None, None, None));
    };
    decode_profiles(&text).map_err(|error| {
        // The next save rewrites this path, so moving the unparsable file aside
        // first is what keeps a profile list a single bad byte would cost.
        let kept = path.with_file_name(format!("{PROFILES_FILE}.broken"));
        match fs::rename(&path, &kept) {
            Ok(()) => format!(
                "Could not read {} as TOML, and it has been kept as {}: {error}",
                path.display(),
                kept.display()
            ),
            Err(rename_error) => format!(
                "Could not read {} as TOML, and it could not be moved aside ({rename_error}): {error}",
                path.display()
            ),
        }
    })
}

fn decode_profiles(text: &str) -> Result<Restored, String> {
    toml::from_str::<ProfileFile>(text)
        .map(|file| (file.profiles, file.active, file.fonts, file.settings))
        .map_err(|error| error.to_string())
}

pub fn save_profiles(
    profiles: &[StoredProfile],
    active: Option<&str>,
    fonts: &StoredFonts,
    settings: &StoredSettings,
) -> Result<(), String> {
    let text = toml::to_string_pretty(&ProfileFile {
        active: active.map(str::to_string),
        fonts: Some(fonts.clone()),
        settings: Some(settings.clone()),
        profiles: profiles.to_vec(),
    })
    .map_err(|error| format!("Could not encode the profile list: {error}"))?;
    write_file(&dbdelve_directory()?.join(PROFILES_FILE), &text)
}

pub fn profile_id(name: &str, existing: &[String]) -> String {
    let slug: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let parts = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let base = if parts.is_empty() {
        "profile".to_string()
    } else {
        parts.join("-")
    };

    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.iter().any(|id| id == &candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// `Ok(None)` only for a keychain that holds no item for this profile. A denied
/// prompt, a locked keychain and a secret that is not text are failures: a
/// blank password is valid, so none of them can be inferred from the connect
/// attempt that would follow.
pub fn password(profile_id: &str) -> Result<Option<String>, String> {
    match keychain_entry(profile_id)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(keyring::Error::BadEncoding(_)) => {
            Err("The keychain password is not valid text.".to_string())
        }
        Err(error) => Err(format!(
            "Could not read the password from the keychain: {error}"
        )),
    }
}

pub fn set_password(profile_id: &str, password: &str) -> Result<(), String> {
    keychain_entry(profile_id)?
        .set_password(password)
        .map_err(|error| format!("Could not save the password to the keychain: {error}"))
}

pub fn delete_password(profile_id: &str) {
    if let Ok(entry) = keychain_entry(profile_id) {
        let _ = entry.delete_credential();
    }
}

/// Keychain Services on macOS, Secret Service on Linux. The service is the
/// variant name and the account the profile id, which is what keeps a dev
/// build's passwords apart from a release build's.
fn keychain_entry(profile_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(&variant_name()?, profile_id)
        .map_err(|error| format!("Could not reach the keychain: {error}"))
}

pub fn saved_queries(profile_id: &str) -> Vec<String> {
    let Ok(directory) = query_directory(profile_id) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".sql")?
                .to_string();
            validate_query_name(&name).ok()?;
            Some(name)
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub fn read_query(profile_id: &str, name: &str) -> Result<Option<String>, String> {
    read_file(&query_path(profile_id, name)?)
}

pub fn write_query(profile_id: &str, name: &str, sql: &str) -> Result<(), String> {
    write_file(&query_path(profile_id, name)?, sql)
}

/// Every query a profile saved, on its way out with the profile.
///
/// `profile_id` derives from the name, so a profile recreated under the name of
/// a removed one is handed the same id -- and would open a dead profile's
/// queries as its own. Leaving the directory behind is what makes that happen.
pub fn delete_queries(profile_id: &str) -> Result<(), String> {
    let directory = query_directory(profile_id)?;
    match fs::remove_dir_all(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not delete {}: {error}", directory.display())),
    }
}

pub fn delete_query(profile_id: &str, name: &str) -> Result<(), String> {
    let path = query_path(profile_id, name)?;
    fs::remove_file(&path).map_err(|error| format!("Could not delete {}: {error}", path.display()))
}

pub fn validate_query_name(name: &str) -> Result<(), String> {
    match unsafe_component(name) {
        Some(reason) => Err(format!("Query name {reason}.")),
        None => Ok(()),
    }
}

/// The unsaved buffer for one tab. Tab 0 falls back to the single file a build
/// before per-tab buffers wrote, so a profile from such a build comes back with
/// its buffer rather than an empty editor.
///
/// The legacy file is read, never removed: the next write lands in the new
/// name, and deleting the old one would take the buffer away from an older
/// build run against the same directory afterwards.
pub fn read_scratch(profile_id: &str, tab: u64) -> Result<Option<String>, String> {
    let directory = query_directory(profile_id)?;
    match read_file(&directory.join(scratch_file(tab)))? {
        Some(sql) => Ok(Some(sql)),
        None if tab == 0 => read_file(&directory.join(LEGACY_SCRATCH_FILE)),
        None => Ok(None),
    }
}

pub fn write_scratch(profile_id: &str, tab: u64, sql: &str) -> Result<(), String> {
    write_file(&query_directory(profile_id)?.join(scratch_file(tab)), sql)
}

/// A file that is not there is not a failure: closing a buffer nothing was ever
/// typed into leaves no file to delete.
pub fn delete_scratch(profile_id: &str, tab: u64) -> Result<(), String> {
    let path = query_directory(profile_id)?.join(scratch_file(tab));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not delete {}: {error}", path.display())),
    }
}

/// One statement on its way into a profile's history, newest at the end.
///
/// A JSON string per line rather than the SQL itself: a statement holds
/// newlines, semicolons and comments, so there is no separator to put between
/// two of them that is not also SQL. Appended rather than rewritten, so a run
/// costs one write and no history can be lost to a rewrite that failed
/// halfway -- see [`compact_history`] for the one rewrite that does happen.
pub fn append_history(profile_id: &str, sql: &str) -> Result<(), String> {
    let directory = query_directory(profile_id)?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;
    let path = directory.join(HISTORY_FILE);
    let line = serde_json::to_string(sql)
        .map_err(|error| format!("Could not encode the statement: {error}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    secure(&path)?;
    writeln!(file, "{line}")
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    compact_history(&path);
    Ok(())
}

/// Drops everything past [`HISTORY_DEPTH`] once the file has run
/// [`HISTORY_SLACK`] lines long, so the append in `append_history` cannot grow
/// it without bound.
///
/// Written through [`write_file`] rather than in place: the same
/// temporary-then-rename that protects every other file here, so a compaction
/// that fails halfway leaves the history it was trimming intact.
///
/// The rewrite is what [`decode_history`] would have returned anyway -- newest
/// first, each statement once -- put back oldest first so a later read walks
/// it the same way. Failures are dropped: the history is still correct to read,
/// it just costs the disk it already had.
fn compact_history(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    if text.lines().count() <= HISTORY_SLACK {
        return;
    }
    let mut kept = String::new();
    for sql in decode_history(&text).iter().rev() {
        let Ok(line) = serde_json::to_string(sql) else {
            return;
        };
        kept.push_str(&line);
        kept.push('\n');
    }
    let _ = write_file(path, &kept);
}

/// What this profile has run, newest first and each statement once. A missing
/// or unreadable file reads as no history: there is nothing here that the user
/// wrote and cannot get back another way.
pub fn history(profile_id: &str) -> Vec<String> {
    let Ok(directory) = query_directory(profile_id) else {
        return Vec::new();
    };
    match fs::read_to_string(directory.join(HISTORY_FILE)) {
        Ok(text) => decode_history(&text),
        Err(_) => Vec::new(),
    }
}

/// A line that will not decode is skipped rather than ending the read: the file
/// is appended to on every run, and the half-written last line a crash leaves
/// behind is not a reason to lose everything before it.
fn decode_history(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|sql| seen.insert(sql.clone()))
        .take(HISTORY_DEPTH)
        .collect()
}

/// A missing or unreadable file reads as no cached grid: a stale or corrupt
/// snapshot is worth exactly as much as none, and must never be the reason
/// the app fails to start.
/// When a snapshot was taken, and the clock a snapshot's age is measured
/// against. A clock set before the epoch reads as 0 rather than refusing to
/// write a grid over it.
pub fn captured_at() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

pub fn read_grid(profile_id: &str, key: &str) -> Option<StoredGrid> {
    let path = grid_path(profile_id, key).ok()?;
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes the first [`GRID_ROW_CAP`] rows atomically; `total_rows` is kept as
/// given, so a capped snapshot still reports the query's real size.
pub fn write_grid(profile_id: &str, key: &str, grid: &StoredGrid) -> Result<(), String> {
    let path = grid_path(profile_id, key)?;
    let capped;
    let grid = if grid.rows.len() > GRID_ROW_CAP {
        // Field by field rather than `..grid.clone()`: that clones the whole
        // row vector -- tens of megabytes, on the frame thread -- to keep the
        // first few thousand of it and drop the rest.
        capped = StoredGrid {
            columns: grid.columns.clone(),
            rows: grid.rows[..GRID_ROW_CAP].to_vec(),
            total_rows: grid.total_rows,
            sort: grid.sort.clone(),
            order_by: grid.order_by.clone(),
            widths: grid.widths.clone(),
            active: grid.active,
            last_query: grid.last_query.clone(),
            limit: grid.limit,
            filter: grid.filter.clone(),
            showing_structure: grid.showing_structure,
            captured: grid.captured,
        };
        &capped
    } else {
        grid
    };
    let text = serde_json::to_string(grid)
        .map_err(|error| format!("Could not encode the grid: {error}"))?;
    write_file(&path, &text)
}

/// Every grid snapshot a profile cached, on its way out with the profile.
///
/// `profile_id` derives from the name, so a profile recreated under the name
/// of a removed one is handed the same id -- and would read a dead profile's
/// cached rows as its own. Leaving the directory behind is what makes that
/// happen.
pub fn delete_grids(profile_id: &str) -> Result<(), String> {
    let directory = grids_directory(profile_id)?;
    match fs::remove_dir_all(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not delete {}: {error}", directory.display())),
    }
}

/// Snapshots that belong to no tab any more.
///
/// A tab's key is made of what identifies it -- a query buffer's id, a
/// relation's schema and name -- so renaming a table strands its snapshot under
/// the old name with nothing left to remove it by. `live` is every key the
/// profile on disk still has a tab for, so a file that is in use is never the
/// one deleted. Failures are dropped: this is cache housekeeping, and a file
/// that could not be read or removed costs disk, not work.
pub fn prune_grids(profile_id: &str, live: &HashSet<String>) {
    let Ok(directory) = grids_directory(profile_id) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only the snapshots themselves: a `.tmp` beside one is `write_file`
        // mid-write, and removing that is a different bug.
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(key) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !live.contains(key) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// A snapshot nothing ever wrote has no file to remove.
pub fn remove_grid(profile_id: &str, key: &str) -> Result<(), String> {
    let path = grid_path(profile_id, key)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not delete {}: {error}", path.display())),
    }
}

/// A query tab's grid key. The id alone is enough -- it is already a bare
/// integer, so nothing in it can collide with the escaping [`object_grid_key`]
/// does for schema and relation names.
pub fn query_grid_key(id: u64) -> String {
    format!("q-{id}")
}

/// An object tab's grid key. A schema or relation name can hold anything --
/// `/`, `.`, spaces, non-ASCII -- so every byte outside `[A-Za-z0-9_-]` is
/// percent-encoded, `.` included: it is this function's own separator, and a
/// name that happened to contain one would otherwise let two different tables
/// collide on one key (`"a.b"` + `"c"` and `"a"` + `"b.c"` would both read
/// `"a.b.c"` if `.` were left unescaped). The filter is a third component
/// under the same escaping, because it is part of the tab's identity.
///
/// An empty filter emits the two-component key it always has: a third
/// component on every key would orphan every snapshot already on disk, and
/// every restored relation tab would open empty and re-query.
pub fn object_grid_key(schema: &str, name: &str, filter: &str) -> String {
    let key = format!("o-{}.{}", escape_grid_key(schema), escape_grid_key(name));
    if filter.is_empty() {
        key
    } else {
        format!("{key}.{}", escape_grid_key(filter))
    }
}

fn escape_grid_key(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

fn unsafe_component(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        Some("is empty")
    } else if value.contains(['/', '\\']) {
        Some("contains a path separator")
    } else if value.contains('\0') {
        Some("contains a NUL character")
    } else if value.starts_with('.') {
        Some("starts with a dot")
    } else {
        None
    }
}

/// `dbdelve` unset or empty, `dbdelve-<variant>` otherwise.
///
/// The support directory and the keychain service both come from here, and
/// they have to move together: a dev build that suffixed only one of them
/// would read the release's saved passwords or write its `profiles.toml`.
/// A variant that cannot be a path component is an error rather than a
/// fallback to the release name, for the same reason.
fn variant_name() -> Result<String, String> {
    let Some(variant) = std::env::var("DBDELVE_VARIANT")
        .ok()
        .filter(|variant| !variant.is_empty())
    else {
        return Ok(NAME.to_string());
    };
    if let Some(reason) = unsafe_component(&variant) {
        return Err(format!("DBDELVE_VARIANT {reason}."));
    }
    Ok(format!("{NAME}-{variant}"))
}

/// macOS keeps Application Support, where every install before this already
/// has its data. Everywhere else follows the XDG base directory spec.
fn dbdelve_directory() -> Result<PathBuf, String> {
    let variant = variant_name()?;
    #[cfg(target_os = "macos")]
    let root = home()?.join("Library/Application Support");
    #[cfg(not(target_os = "macos"))]
    let root = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(data_home) => PathBuf::from(data_home),
        None => home()?.join(".local/share"),
    };
    Ok(root.join(variant))
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set.".to_string())
}

fn query_directory(profile_id: &str) -> Result<PathBuf, String> {
    if let Some(reason) = unsafe_component(profile_id) {
        return Err(format!("Profile id {reason}."));
    }
    Ok(dbdelve_directory()?.join("queries").join(profile_id))
}

fn scratch_file(tab: u64) -> String {
    format!(".scratch-{tab}.sql")
}

fn query_path(profile_id: &str, name: &str) -> Result<PathBuf, String> {
    validate_query_name(name)?;
    Ok(query_directory(profile_id)?.join(format!("{name}.sql")))
}

fn grids_directory(profile_id: &str) -> Result<PathBuf, String> {
    if let Some(reason) = unsafe_component(profile_id) {
        return Err(format!("Profile id {reason}."));
    }
    Ok(dbdelve_directory()?.join("grids").join(profile_id))
}

fn grid_path(profile_id: &str, key: &str) -> Result<PathBuf, String> {
    if let Some(reason) = unsafe_component(key) {
        return Err(format!("Grid key {reason}."));
    }
    Ok(grids_directory(profile_id)?.join(format!("{key}.json")))
}

/// `Ok(None)` for a file that is not there, which every caller has a sensible
/// answer for. A file that exists and could not be read does not get the same
/// answer -- that is the one that sends a person looking for lost work.
fn read_file(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, contents)
        .map_err(|error| format!("Could not write {}: {error}", temporary.display()))?;
    // Set before the rename, not after: the rename is what makes this the file
    // at `path`, so a mode applied afterward would leave it world-readable for
    // however long the two steps are apart.
    let written = secure(&temporary).and_then(|()| {
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not replace {}: {error}", path.display()))
    });
    if written.is_err() {
        // Nothing reads a leftover temporary, and every failed write would
        // otherwise leave one behind for good.
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Connection settings live in these files -- host, port, user, database --
/// so `0600` holds regardless of whether `path` is being created or replaced.
/// `OpenOptions::mode` only sets this at creation, which is not enough for a
/// file that already existed with looser permissions from before this rule.
fn secure(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not set permissions on {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ConnectionColor;

    /// `HOME` is process-wide and the tests run in threads, so the ones that
    /// touch the disk take turns and each gets its own directory to be the
    /// whole of dbdelve's storage for the length of the test.
    fn with_home<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<u32> = std::sync::Mutex::new(0);

        let mut counter = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        *counter += 1;
        let home = std::env::temp_dir().join(format!("dbdelve-store-test-{}", *counter));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("the test home must be creatable");

        let previous = std::env::var_os("HOME");
        // `XDG_DATA_HOME` goes with it: left set, it would point the storage
        // outside the test home on every platform that honours it.
        let previous_data_home = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: the lock above is what makes this the only thread reading or
        // writing the environment for as long as `body` runs.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("XDG_DATA_HOME");
        }
        let outcome = body();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            if let Some(value) = previous_data_home {
                std::env::set_var("XDG_DATA_HOME", value);
            }
        }
        let _ = fs::remove_dir_all(&home);
        outcome
    }

    /// What `dbdelve_directory` appends to the home the test set, which is
    /// the platform's data directory and not one fixed path.
    fn data_path(variant: &str) -> PathBuf {
        if cfg!(target_os = "macos") {
            PathBuf::from("Library/Application Support").join(variant)
        } else {
            PathBuf::from(".local/share").join(variant)
        }
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    /// The whole backward-compatibility story: a file written before modes
    /// existed has neither key, which is what `restore_profile` reads as
    /// Read-write with nothing silenced.
    #[test]
    fn a_profile_without_a_mode_loads_as_read_write() {
        with_home(|| {
            let toml = r#"
active = "local"

[[profiles]]
id = "local"
name = "local"
host = "localhost"
database = "postgres"
user = "shayan"
"#;
            let path = dbdelve_directory().unwrap().join(PROFILES_FILE);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, toml).unwrap();

            let (profiles, ..) = load_profiles().unwrap();
            let profile = &profiles[0];
            assert_eq!(profile.mode, None);
            assert!(profile.confirmed.is_empty());
        });
    }

    /// A value from a build this one is older than costs that one field, not
    /// every connection in the file. Both fields were typed enums until this
    /// test existed, so an unknown mode failed `load_profiles` outright and the
    /// user lost the lot.
    #[test]
    fn a_stored_mode_this_build_cannot_read_does_not_take_the_file_with_it() {
        with_home(|| {
            let toml = r#"
active = "local"

[[profiles]]
id = "local"
name = "local"
host = "localhost"
database = "postgres"
user = "shayan"
mode = "read-append"
confirmed = ["drop", "shred"]

[[profiles]]
id = "other"
name = "other"
host = "localhost"
database = "postgres"
user = "shayan"
"#;
            let path = dbdelve_directory().unwrap().join(PROFILES_FILE);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, toml).unwrap();

            let (profiles, ..) = load_profiles().unwrap();
            assert_eq!(profiles.len(), 2);
            // Kept raw here; `restore_profile` is where an unreadable slug
            // becomes a mode, and where it decides which way to fail.
            assert_eq!(profiles[0].mode.as_deref(), Some("read-append"));
            assert_eq!(profiles[0].confirmed, vec!["drop", "shred"]);
        });
    }

    #[test]
    fn a_mode_and_its_silenced_kinds_survive_a_round_trip() {
        with_home(|| {
            let stored = StoredProfile {
                id: "dev".into(),
                name: "Dev".into(),
                host: "127.0.0.1".into(),
                port: Some(5432),
                database: "dbdelve_dev".into(),
                user: "dbdelve".into(),
                sslmode: Some("verify-full".into()),
                root_certificate: None,
                engine: Some("postgres".into()),
                path: None,
                account: None,
                private_key: None,
                warehouse: None,
                role: None,
                editor_font_size: None,
                statement_timeout: Some(30),
                next_query_id: Some(1),
                color: None,
                mode: Some("full".into()),
                confirmed: vec!["drop".into(), "truncate".into()],
                open_query: None,
                open_queries: Vec::new(),
                open_objects: Vec::new(),
            };

            save_profiles(
                std::slice::from_ref(&stored),
                Some(&stored.id),
                &StoredFonts::default(),
                &StoredSettings::default(),
            )
            .unwrap();

            let (profiles, ..) = load_profiles().unwrap();
            assert_eq!(profiles[0], stored);
        });
    }

    #[test]
    fn a_profile_survives_the_round_trip_through_toml() {
        // TOML refuses a scalar written after a table, so the open-object list
        // has to stay the last field -- and that is invisible until it fails.
        let profile = StoredProfile {
            id: "dev".into(),
            name: "Dev".into(),
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "dbdelve_dev".into(),
            user: "dbdelve".into(),
            sslmode: Some("verify-full".into()),
            root_certificate: Some("/etc/ssl/rds.pem".into()),
            engine: Some("postgres".into()),
            path: None,
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: Some(16.0),
            statement_timeout: Some(30),
            next_query_id: Some(7),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: Some("daily".into()),
            open_queries: Vec::new(),
            open_objects: vec![
                StoredObject {
                    schema: "public".into(),
                    name: "accounts".into(),
                    routine: false,
                    kind: RelationKind::MaterializedView,
                    filter: String::new(),
                    filters: Vec::new(),
                    active: true,
                    bars: Vec::new(),
                },
                StoredObject {
                    schema: "public".into(),
                    name: "total(integer)".into(),
                    routine: true,
                    kind: RelationKind::default(),
                    filter: String::new(),
                    filters: Vec::new(),
                    active: false,
                    bars: Vec::new(),
                },
            ],
        };
        let file = ProfileFile {
            fonts: None,
            active: Some("dev".into()),
            settings: None,
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
        assert_eq!(decoded.active.as_deref(), Some("dev"));
    }

    #[test]
    fn a_profile_written_before_a_field_existed_still_loads() {
        // Every field added after the first release is `serde(default)`, and this
        // is the file already on disk for anyone who has run dbdelve before. A
        // decode error here reads as "no profiles", which is what the next save
        // would then write back.
        let (profiles, active, ..) = decode_profiles(
            "\
[[profiles]]
id = \"dbdelve-dev\"
name = \"dbdelve_dev\"
host = \"127.0.0.1\"
port = 55432
database = \"dbdelve_dev\"
user = \"dbdelve\"
sslmode = \"prefer\"
open_objects = []
",
        )
        .expect("a profile predating the optional fields must load");

        assert_eq!(active, None);
        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        assert_eq!(profile.editor_font_size, None);
        assert_eq!(profile.root_certificate, None);
        assert_eq!(profile.open_query, None);
        assert_eq!(profile.color, None);
        // No limit, which is what it was running with.
        assert_eq!(profile.statement_timeout, None);
    }

    #[test]
    fn a_colour_survives_the_round_trip_through_toml() {
        // The colour is a scalar and the object list is a table, so a colour
        // declared after the lists encodes fine and then fails to decode.
        let profile = StoredProfile {
            id: "dev".into(),
            name: "Dev".into(),
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "dbdelve_dev".into(),
            user: "dbdelve".into(),
            sslmode: None,
            root_certificate: None,
            engine: Some("postgres".into()),
            path: None,
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: None,
            statement_timeout: None,
            next_query_id: Some(0),
            color: Some(ConnectionColor::Purple.slug().to_string()),
            mode: None,
            confirmed: Vec::new(),
            open_query: None,
            open_queries: Vec::new(),
            open_objects: vec![StoredObject {
                schema: "public".into(),
                name: "accounts".into(),
                routine: false,
                kind: RelationKind::Table,
                filter: String::new(),
                filters: Vec::new(),
                active: true,
                bars: Vec::new(),
            }],
        };
        let file = ProfileFile {
            fonts: None,
            active: None,
            settings: None,
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn every_colour_survives_its_slug() {
        for colour in ConnectionColor::ALL {
            assert_eq!(ConnectionColor::from_slug(colour.slug()), Some(colour));
        }
    }

    #[test]
    fn a_slug_dbdelve_cannot_read_is_no_colour() {
        assert_eq!(ConnectionColor::from_slug("chartreuse"), None);
        assert_eq!(ConnectionColor::from_slug(""), None);
    }

    #[test]
    fn a_sqlite_profile_survives_the_round_trip_through_toml() {
        // SQLite has no host, port, user or TLS -- a profile for it writes
        // those fields blank rather than omitting them, since `StoredProfile`
        // stays one shape for every engine.
        let profile = StoredProfile {
            id: "local".into(),
            name: "Local".into(),
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            sslmode: None,
            root_certificate: None,
            engine: Some("sqlite".into()),
            path: Some("/Users/dev/dbdelve_dev.db".into()),
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: Some(14.0),
            statement_timeout: None,
            next_query_id: Some(7),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: None,
            open_queries: Vec::new(),
            open_objects: vec![StoredObject {
                schema: "main".into(),
                name: "accounts".into(),
                routine: false,
                kind: RelationKind::Table,
                filter: String::new(),
                filters: Vec::new(),
                active: true,
                bars: Vec::new(),
            }],
        };
        let file = ProfileFile {
            active: None,
            fonts: None,
            settings: None,
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn a_snowflake_profile_survives_the_round_trip_through_toml() {
        // The key is a path, so it is a profile field like any other and the
        // round trip is the whole of what persists for this engine.
        let profile = StoredProfile {
            id: "warehouse".into(),
            name: "Warehouse".into(),
            host: String::new(),
            port: None,
            database: "ANALYTICS".into(),
            user: "TIM".into(),
            sslmode: None,
            root_certificate: None,
            engine: Some("snowflake".into()),
            path: None,
            account: Some("myorg-myaccount".into()),
            private_key: Some("/Users/dev/.ssh/snowflake.p8".into()),
            warehouse: Some("COMPUTE_WH".into()),
            role: None,
            editor_font_size: None,
            statement_timeout: Some(60),
            next_query_id: Some(1),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: None,
            open_queries: Vec::new(),
            open_objects: Vec::new(),
        };
        let file = ProfileFile {
            active: None,
            fonts: None,
            settings: None,
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn an_object_written_before_the_kind_existed_loads_as_a_table() {
        // A relation's tab is opened from what is on disk now, so a profile
        // written before the kind was stored has to name a kind anyway.
        let (profiles, ..) = decode_profiles(
            "\
[[profiles]]
id = \"dbdelve-dev\"
name = \"dbdelve_dev\"
host = \"127.0.0.1\"
database = \"dbdelve_dev\"
user = \"dbdelve\"

[[profiles.open_objects]]
schema = \"public\"
name = \"accounts\"
active = true
",
        )
        .expect("a profile predating the object kind must load");

        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        let [object] = &profile.open_objects[..] else {
            panic!("expected exactly one open object");
        };
        assert_eq!(object.kind, RelationKind::Table);
        assert!(!object.routine);
    }

    #[test]
    fn a_profile_written_before_engine_existed_still_loads() {
        // Every profile on disk before a second engine existed has no `engine`
        // key at all, and must load as Postgres -- which is what it was
        // connecting as -- rather than fail to decode.
        let (profiles, ..) = decode_profiles(
            "\
[[profiles]]
id = \"dbdelve-dev\"
name = \"dbdelve_dev\"
host = \"127.0.0.1\"
port = 55432
database = \"dbdelve_dev\"
user = \"dbdelve\"
sslmode = \"prefer\"
open_objects = []
",
        )
        .expect("a profile predating engine must load");

        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        assert_eq!(profile.engine, None);
        assert_eq!(profile.path, None);
    }

    #[test]
    fn engine_and_path_precede_the_open_objects_table() {
        // TOML cannot emit a scalar after a table, so `engine` and `path` have
        // to sit before `open_objects` in the struct. This is the check that
        // would actually fail if someone reordered it.
        let profile = StoredProfile {
            id: "local".into(),
            name: "Local".into(),
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            sslmode: None,
            root_certificate: None,
            engine: Some("sqlite".into()),
            path: Some("/tmp/dev.sqlite".into()),
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: None,
            statement_timeout: Some(30),
            next_query_id: Some(7),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: None,
            open_queries: Vec::new(),
            open_objects: vec![StoredObject {
                schema: "main".into(),
                name: "accounts".into(),
                routine: false,
                kind: RelationKind::Table,
                filter: String::new(),
                filters: Vec::new(),
                active: true,
                bars: Vec::new(),
            }],
        };
        let text = toml::to_string_pretty(&ProfileFile {
            active: None,
            fonts: None,
            settings: None,
            profiles: vec![profile],
        })
        .expect("profile must encode");

        let engine_at = text.find("engine = ").expect("engine must be written");
        let timeout_at = text
            .find("statement_timeout = ")
            .expect("statement_timeout must be written");
        let path_at = text.find("path = ").expect("path must be written");
        let open_objects_at = text
            .find("[[profiles.open_objects]]")
            .expect("open_objects must be written as a table");

        assert!(
            engine_at < open_objects_at,
            "engine after open_objects:\n{text}"
        );
        assert!(
            path_at < open_objects_at,
            "path after open_objects:\n{text}"
        );
        assert!(
            timeout_at < open_objects_at,
            "statement_timeout after open_objects:\n{text}"
        );
    }

    #[test]
    fn the_profile_file_round_trips_and_a_file_without_settings_reads_as_none() {
        // An empty `profiles` list is not a real test of table order: toml 0.9
        // hoists it to a scalar `profiles = []` above `[settings]` regardless
        // of field order, so a decode-only check against it proves nothing.
        // This encodes two real profiles alongside `fonts` and `settings` and
        // asserts the round trip, which is what would actually fail if a field
        // were moved to where TOML cannot place it.
        let file = ProfileFile {
            active: Some("dev".into()),
            fonts: Some(StoredFonts {
                chrome: Some("Inter".into()),
                editor: Some("Berkeley Mono".into()),
                grid: Some("Inter".into()),
            }),
            settings: Some(StoredSettings {
                theme: Some("Dark".into()),
                editor_font_size: Some(18.0),
                preview_rows: Some(500),
                opacity: Some(0.8),
                custom_keybindings: Some(HashMap::from([(
                    "apply_edits".to_string(),
                    "cmd-shift-s".to_string(),
                )])),
            }),
            profiles: vec![
                StoredProfile {
                    id: "dev".into(),
                    name: "Dev".into(),
                    host: "localhost".into(),
                    port: Some(5432),
                    database: "dev".into(),
                    user: "dbdelve".into(),
                    sslmode: Some("prefer".into()),
                    root_certificate: None,
                    engine: Some("postgres".into()),
                    path: None,
                    account: None,
                    private_key: None,
                    warehouse: None,
                    role: None,
                    editor_font_size: None,
                    statement_timeout: Some(30),
                    next_query_id: Some(2),
                    color: None,
                    mode: None,
                    confirmed: Vec::new(),
                    open_query: None,
                    open_queries: Vec::new(),
                    open_objects: Vec::new(),
                },
                StoredProfile {
                    id: "local".into(),
                    name: "Local".into(),
                    host: String::new(),
                    port: None,
                    database: String::new(),
                    user: String::new(),
                    sslmode: None,
                    root_certificate: None,
                    engine: Some("sqlite".into()),
                    path: Some("/tmp/dev.sqlite".into()),
                    account: None,
                    private_key: None,
                    warehouse: None,
                    role: None,
                    editor_font_size: None,
                    statement_timeout: None,
                    next_query_id: None,
                    color: None,
                    mode: None,
                    confirmed: Vec::new(),
                    open_query: None,
                    open_queries: Vec::new(),
                    open_objects: Vec::new(),
                },
            ],
        };

        let text = toml::to_string_pretty(&file).expect("profile file must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profile file must decode");
        assert_eq!(
            decoded, file,
            "round trip did not preserve the file:\n{text}"
        );

        // A file written before `settings` existed has no `[settings]` table
        // at all, and that is what is on disk for everyone running dbdelve
        // today -- it has to keep reading as `None`, not as the defaults.
        let (.., missing) =
            decode_profiles("active = \"dev\"\n").expect("a file predating settings must load");
        assert_eq!(missing, None);
    }

    #[test]
    fn an_unparsable_profile_file_is_an_error_rather_than_an_empty_list() {
        // The empty list is what the next save writes back, so a parse error
        // that reads as "no profiles" is a parse error that deletes them.
        assert!(decode_profiles("host = ").is_err());
        assert_eq!(decode_profiles(""), Ok((Vec::new(), None, None, None)));
    }

    #[test]
    fn profile_ids_are_safe_and_stable() {
        assert_eq!(profile_id("Prod (EU West)", &[]), "prod-eu-west");
        assert_eq!(profile_id("///", &[]), "profile");
        assert_eq!(profile_id("Prod", &ids(&["prod", "prod-2"])), "prod-3");
    }

    #[test]
    fn unsafe_query_names_are_rejected() {
        for name in [
            "",
            "   ",
            ".",
            "..",
            ".scratch",
            "../secrets",
            "a/b",
            "a\\b",
            "a\0b",
        ] {
            assert!(validate_query_name(name).is_err(), "{name:?}");
        }
    }

    /// Inside `with_home` for its lock rather than for its directory:
    /// `DBDELVE_VARIANT` is process-wide too, and every other test here reads
    /// it through `dbdelve_directory`.
    #[test]
    fn the_variant_moves_the_directory_and_the_keychain_together() {
        with_home(|| {
            // SAFETY: as in `with_home` -- the lock it holds for the length of
            // this body is what makes the writes single-threaded.
            let variant = |value: Option<&str>| unsafe {
                match value {
                    Some(value) => std::env::set_var("DBDELVE_VARIANT", value),
                    None => std::env::remove_var("DBDELVE_VARIANT"),
                }
            };

            for unset in [None, Some("")] {
                variant(unset);
                assert_eq!(variant_name().unwrap(), "dbdelve");
                let directory = dbdelve_directory().unwrap();
                assert!(directory.ends_with(data_path("dbdelve")));
            }

            variant(Some("dev"));
            assert_eq!(variant_name().unwrap(), "dbdelve-dev");
            let directory = dbdelve_directory().unwrap();
            assert!(directory.ends_with(data_path("dbdelve-dev")));

            // No NUL case: `set_var` panics on one before the code under test
            // ever sees it.
            for rejected in ["../release", "a/b", "a\\b", ".dev", "   "] {
                variant(Some(rejected));
                assert!(variant_name().is_err(), "{rejected:?}");
                assert!(dbdelve_directory().is_err(), "{rejected:?}");
            }

            variant(None);
        });
    }

    #[test]
    fn a_history_file_reads_back_newest_first_and_each_statement_once() {
        // The multi-line statement is the point of the encoding: a raw-SQL file
        // has no separator between two statements that is not also SQL.
        let text = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string("SELECT 1").unwrap(),
            serde_json::to_string("SELECT\n  *\nFROM accounts; -- all").unwrap(),
            serde_json::to_string("SELECT 1").unwrap(),
        );

        assert_eq!(
            decode_history(&text),
            ["SELECT 1", "SELECT\n  *\nFROM accounts; -- all"]
        );
    }

    #[test]
    fn a_half_written_line_does_not_take_the_history_with_it() {
        let text = format!("{}\n\"SELECT 2", serde_json::to_string("SELECT 1").unwrap());

        assert_eq!(decode_history(&text), ["SELECT 1"]);
    }

    #[test]
    fn a_long_lived_history_stops_growing_at_the_slack() {
        with_home(|| {
            // One past the slack, so the last append is the one that has to
            // trip the compaction.
            let runs = HISTORY_SLACK + 1;
            for n in 0..runs {
                append_history("dev", &format!("SELECT {n}")).unwrap();
            }

            let path = query_directory("dev").unwrap().join(HISTORY_FILE);
            let text = fs::read_to_string(&path).unwrap();
            assert_eq!(text.lines().count(), HISTORY_DEPTH);

            // Trimmed off the front, so what the user can still reach is the
            // newest HISTORY_DEPTH runs and not some older window of them.
            let read_back = history("dev");
            assert_eq!(read_back.len(), HISTORY_DEPTH);
            assert_eq!(read_back[0], format!("SELECT {}", runs - 1));
            assert_eq!(
                read_back[HISTORY_DEPTH - 1],
                format!("SELECT {}", runs - HISTORY_DEPTH)
            );

            // The rewrite goes through `write_file`, so the mode the appends
            // set has to survive it.
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        });
    }

    #[test]
    fn a_written_config_file_is_readable_only_by_its_owner() {
        // Profiles, saved queries and the scratch buffer all hold connection
        // settings and go through this one function, so this is the one place
        // that has to prove the permission rather than every caller.
        let path = std::env::temp_dir().join("dbdelve-store-permissions-test.toml");
        let _ = fs::remove_file(&path);

        write_file(&path, "host = \"example\"").expect("file must write");

        let mode = fs::metadata(&path)
            .expect("file must exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn each_tab_keeps_its_own_scratch_buffer() {
        with_home(|| {
            write_scratch("dev", 0, "SELECT 1").expect("tab 0 must write");
            write_scratch("dev", 7, "SELECT 7").expect("tab 7 must write");

            assert_eq!(read_scratch("dev", 0), Ok(Some("SELECT 1".to_string())));
            assert_eq!(read_scratch("dev", 7), Ok(Some("SELECT 7".to_string())));
            assert_eq!(read_scratch("dev", 3), Ok(None));
        });
    }

    #[test]
    fn tab_zero_reads_the_buffer_an_older_build_left_behind() {
        // The single `.scratch.sql` is every profile on disk today. Read as tab
        // 0's buffer until a write moves it, and preferred against only when
        // the new name exists -- otherwise a person's unsaved work is an empty
        // editor after an upgrade.
        with_home(|| {
            let legacy = query_directory("dev")
                .expect("the id must be safe")
                .join(LEGACY_SCRATCH_FILE);
            write_file(&legacy, "SELECT legacy").expect("the legacy file must write");

            assert_eq!(
                read_scratch("dev", 0),
                Ok(Some("SELECT legacy".to_string()))
            );
            // Only tab 0 inherits it.
            assert_eq!(read_scratch("dev", 1), Ok(None));

            write_scratch("dev", 0, "SELECT new").expect("tab 0 must write");

            assert_eq!(read_scratch("dev", 0), Ok(Some("SELECT new".to_string())));
            assert!(legacy.exists(), "the legacy file must be left alone");
        });
    }

    #[test]
    fn deleting_one_tabs_scratch_leaves_the_others() {
        with_home(|| {
            write_scratch("dev", 0, "SELECT 0").expect("tab 0 must write");
            write_scratch("dev", 1, "SELECT 1").expect("tab 1 must write");

            assert_eq!(delete_scratch("dev", 1), Ok(()));

            assert_eq!(read_scratch("dev", 0), Ok(Some("SELECT 0".to_string())));
            assert_eq!(read_scratch("dev", 1), Ok(None));
        });
    }

    #[test]
    fn deleting_a_scratch_that_was_never_written_is_not_a_failure() {
        // Closing a buffer nobody typed into deletes nothing, and that is the
        // ordinary case rather than an error.
        with_home(|| {
            assert_eq!(delete_scratch("dev", 4), Ok(()));
        });
    }

    #[test]
    fn removing_a_profile_takes_every_tabs_scratch_with_it() {
        // A profile id derives from its name, so a recreated profile can be
        // handed a dead one's id -- and a leftover buffer would open as
        // somebody else's SQL.
        with_home(|| {
            write_scratch("dev", 0, "SELECT 0").expect("tab 0 must write");
            write_scratch("dev", 2, "SELECT 2").expect("tab 2 must write");
            let legacy = query_directory("dev")
                .expect("the id must be safe")
                .join(LEGACY_SCRATCH_FILE);
            write_file(&legacy, "SELECT legacy").expect("the legacy file must write");

            delete_queries("dev").expect("the profile's queries must be removable");

            assert_eq!(read_scratch("dev", 0), Ok(None));
            assert_eq!(read_scratch("dev", 2), Ok(None));
            assert!(!legacy.exists(), "the legacy file must go too");
        });
    }

    #[test]
    fn a_profile_written_before_open_queries_existed_keeps_its_open_query() {
        // The file on disk for anyone running dbdelve today: one buffer, named in
        // `open_query`. Dropping either the field or the decode loses it.
        let (profiles, ..) = decode_profiles(
            "\
[[profiles]]
id = \"dbdelve-dev\"
name = \"dbdelve_dev\"
host = \"127.0.0.1\"
port = 55432
database = \"dbdelve_dev\"
user = \"dbdelve\"
sslmode = \"prefer\"
open_query = \"daily\"
open_objects = []
",
        )
        .expect("a profile predating open_queries must load");

        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        assert_eq!(profile.open_query.as_deref(), Some("daily"));
        assert_eq!(profile.open_queries, vec![]);
    }

    #[test]
    fn open_queries_and_open_objects_both_survive_the_round_trip() {
        // Two arrays of tables in one profile: every scalar has to precede
        // both, and this is the encode that fails if one moves after them.
        let profile = StoredProfile {
            id: "dev".into(),
            name: "Dev".into(),
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "dbdelve_dev".into(),
            user: "dbdelve".into(),
            sslmode: None,
            root_certificate: None,
            engine: Some("postgres".into()),
            path: None,
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: Some(15.0),
            statement_timeout: Some(30),
            next_query_id: Some(7),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: Some("daily".into()),
            open_queries: vec![
                StoredQueryTab {
                    id: 0,
                    name: None,
                    active: false,
                },
                StoredQueryTab {
                    id: 4,
                    name: Some("daily".into()),
                    active: true,
                },
            ],
            open_objects: vec![StoredObject {
                schema: "public".into(),
                name: "accounts".into(),
                routine: false,
                kind: RelationKind::Table,
                filter: String::new(),
                filters: Vec::new(),
                active: true,
                bars: Vec::new(),
            }],
        };
        let text = toml::to_string_pretty(&ProfileFile {
            active: Some("dev".into()),
            fonts: None,
            settings: None,
            profiles: vec![profile.clone()],
        })
        .expect("profiles must encode");

        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");
        assert_eq!(decoded.profiles, vec![profile]);

        let timeout_at = text
            .find("statement_timeout = ")
            .expect("statement_timeout must be written");
        let open_queries_at = text
            .find("[[profiles.open_queries]]")
            .expect("open_queries must be written as a table");
        assert!(
            timeout_at < open_queries_at,
            "statement_timeout after open_queries:\n{text}"
        );
    }

    #[test]
    fn pruning_takes_the_snapshots_no_tab_claims_and_leaves_the_rest() {
        with_home(|| {
            let grid = StoredGrid {
                columns: vec!["n".into()],
                rows: vec![vec![Some("1".into())]],
                total_rows: 1,
                sort: Vec::new(),
                order_by: Vec::new(),
                widths: Vec::new(),
                active: None,
                last_query: None,
                limit: None,
                filter: String::new(),
                showing_structure: false,
                captured: 0,
            };
            let live_query = query_grid_key(0);
            let live_object = object_grid_key("public", "accounts", "");
            // The one a rename stranded: nothing names it any more, and its
            // key cannot be rebuilt from anything that does.
            let orphan = object_grid_key("public", "accounts_old", "");
            for key in [&live_query, &live_object, &orphan] {
                write_grid("dev", key, &grid).expect("a grid must write");
            }

            let live = HashSet::from([live_query.clone(), live_object.clone()]);
            prune_grids("dev", &live);

            assert!(read_grid("dev", &live_query).is_some());
            assert!(read_grid("dev", &live_object).is_some());
            assert_eq!(read_grid("dev", &orphan), None);

            delete_grids("dev").expect("the profile's grids must be removable");
        });
    }

    #[test]
    fn ordinary_query_names_are_accepted() {
        for name in ["daily report", "v1.2 counts", "accounts", "-x-"] {
            assert!(validate_query_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn a_grid_round_trips_caps_its_rows_and_keys_do_not_collide() {
        with_home(|| {
            let small = StoredGrid {
                columns: vec!["id".into(), "name".into()],
                rows: vec![vec![Some("1".into()), None]],
                total_rows: 1,
                sort: vec![(0, true)],
                order_by: vec![("created_at".into(), false)],
                widths: vec![80.0, 160.0],
                active: Some((0, 1)),
                last_query: Some("select * from accounts".into()),
                limit: Some(1_000),
                filter: String::new(),
                showing_structure: false,
                captured: 1_700_000_000,
            };
            let key = query_grid_key(9);
            write_grid("dev", &key, &small).expect("a small grid must write");
            // Whole-struct equality, so a field added without a `serde(default)`
            // -- or dropped from the encode -- fails here rather than silently
            // reading back as nothing.
            assert_eq!(read_grid("dev", &key), Some(small));

            // A result over the cap still writes and reads back, but only the
            // first `GRID_ROW_CAP` rows are on disk -- `total_rows` is what
            // says the rest existed.
            let oversized = StoredGrid {
                columns: vec!["n".into()],
                rows: (0..GRID_ROW_CAP + 10)
                    .map(|n| vec![Some(n.to_string())])
                    .collect(),
                total_rows: GRID_ROW_CAP + 10,
                sort: Vec::new(),
                order_by: Vec::new(),
                widths: Vec::new(),
                active: None,
                last_query: None,
                limit: None,
                filter: String::new(),
                showing_structure: false,
                captured: 0,
            };
            let big_key = query_grid_key(10);
            write_grid("dev", &big_key, &oversized).expect("an oversized grid must write");
            let read_back = read_grid("dev", &big_key).expect("the capped grid must read back");
            assert_eq!(read_back.rows.len(), GRID_ROW_CAP);
            assert_eq!(read_back.total_rows, GRID_ROW_CAP + 10);
            assert_eq!(
                oversized.rows.len(),
                GRID_ROW_CAP + 10,
                "the caller's rows must be untouched"
            );

            // A dot inside a schema or table name must not read as the
            // separator between them.
            assert_ne!(
                object_grid_key("a.b", "c", ""),
                object_grid_key("a", "b.c", "")
            );

            // A profile id derives from its name, so a recreated profile can
            // be handed a dead one's id -- and a leftover snapshot would read
            // as somebody else's rows.
            delete_grids("dev").expect("the profile's grids must be removable");
            assert_eq!(read_grid("dev", &key), None);
            assert_eq!(read_grid("dev", &big_key), None);
        });
    }

    #[test]
    fn a_snapshot_keeps_the_filter_its_rows_were_read_under() {
        with_home(|| {
            let filtered = StoredGrid {
                columns: vec!["id".into()],
                rows: vec![vec![Some("1".into())]],
                total_rows: 1,
                sort: Vec::new(),
                order_by: vec![("\"id\"".into(), true)],
                widths: Vec::new(),
                active: None,
                last_query: None,
                limit: Some(1_000),
                filter: r#""state" = 'ok'"#.into(),
                showing_structure: false,
                captured: 1_700_000_000,
            };
            let key = object_grid_key("public", "accounts", "");
            write_grid("dev", &key, &filtered).expect("a filtered grid must write");

            // Whole-struct equality: the filter is part of what the tab was
            // showing, so a tab that forgot it would come back a different tab.
            assert_eq!(read_grid("dev", &key), Some(filtered));
            delete_grids("dev").expect("the profile's grids must be removable");
        });
    }

    #[test]
    fn a_snapshot_written_before_filters_reads_back_as_unfiltered() {
        // Which is what it was running with. No home needed: this is the decode
        // of a file on someone's disk right now.
        let older = r#"{
            "columns": ["id"],
            "rows": [["1"]],
            "total_rows": 1,
            "captured": 1700000000
        }"#;
        let grid: StoredGrid = serde_json::from_str(older).expect("an older grid must decode");

        assert_eq!(grid.filter, "");
        assert_eq!(grid.limit, None);
    }

    #[test]
    fn a_stored_object_written_before_filters_reads_back_unfiltered() {
        // The tab this profile had open was showing the whole relation, and
        // absent has to mean that rather than fail the decode.
        let (profiles, ..) = decode_profiles(
            "\
[[profiles]]
id = \"dbdelve-dev\"
name = \"dbdelve_dev\"
host = \"127.0.0.1\"
port = 55432
database = \"dbdelve_dev\"
user = \"dbdelve\"

[[profiles.open_objects]]
schema = \"public\"
name = \"accounts\"
",
        )
        .expect("a profile predating the filter must load");

        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        let [object] = &profile.open_objects[..] else {
            panic!("expected exactly one open object");
        };
        assert_eq!(object.filter, "");
        assert!(object.filters.is_empty());
    }

    #[test]
    fn two_tabs_on_one_relation_survive_the_round_trip_as_two() {
        // The whole point of the filter being stored: a restart that collapsed
        // these two into one would lose whichever tab it dropped.
        let profile = StoredProfile {
            id: "dev".into(),
            name: "Dev".into(),
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "dbdelve_dev".into(),
            user: "dbdelve".into(),
            sslmode: None,
            root_certificate: None,
            engine: Some("postgres".into()),
            path: None,
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: None,
            statement_timeout: None,
            next_query_id: Some(0),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: None,
            open_queries: Vec::new(),
            open_objects: vec![
                StoredObject {
                    schema: "public".into(),
                    name: "customers".into(),
                    routine: false,
                    kind: RelationKind::Table,
                    filter: String::new(),
                    filters: Vec::new(),
                    active: false,
                    bars: Vec::new(),
                },
                StoredObject {
                    schema: "public".into(),
                    name: "customers".into(),
                    routine: false,
                    kind: RelationKind::Table,
                    filter: r#""id" = '42'"#.into(),
                    filters: Vec::new(),
                    active: true,
                    bars: vec![StoredFilter {
                        column: "id".into(),
                        value: "42".into(),
                        operator: "equals".into(),
                        conjunction: "AND".into(),
                        raw: false,
                    }],
                },
            ],
        };
        let file = ProfileFile {
            fonts: None,
            active: None,
            settings: None,
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn the_filter_bars_come_back_as_bars_rather_than_as_an_expression() {
        // Nothing parses a `WHERE` back into controls, so the bars themselves
        // have to survive the file -- values with a quote and a separator in
        // them included.
        let object = StoredObject {
            schema: "public".into(),
            name: "customers".into(),
            routine: false,
            kind: RelationKind::Table,
            filter: r#"("state" = 'it''s ok') OR ("tier" LIKE '%2%' ESCAPE '\')"#.into(),
            filters: Vec::new(),
            active: true,
            bars: vec![
                StoredFilter {
                    column: "state".into(),
                    value: "it's ok".into(),
                    operator: "equals".into(),
                    conjunction: "AND".into(),
                    raw: false,
                },
                StoredFilter {
                    column: "tier".into(),
                    value: "2".into(),
                    operator: "contains".into(),
                    conjunction: "OR".into(),
                    raw: false,
                },
            ],
        };
        let profile = StoredProfile {
            id: "dev".into(),
            name: "Dev".into(),
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "dbdelve_dev".into(),
            user: "dbdelve".into(),
            sslmode: None,
            root_certificate: None,
            engine: Some("postgres".into()),
            path: None,
            account: None,
            private_key: None,
            warehouse: None,
            role: None,
            editor_font_size: None,
            statement_timeout: None,
            next_query_id: Some(0),
            color: None,
            mode: None,
            confirmed: Vec::new(),
            open_query: None,
            open_queries: Vec::new(),
            open_objects: vec![object],
        };
        let text = toml::to_string_pretty(&ProfileFile {
            fonts: None,
            active: None,
            settings: None,
            profiles: vec![profile.clone()],
        })
        .expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn an_unfiltered_grid_key_is_the_key_it_has_always_been() {
        // A key that grew a third component unconditionally would orphan every
        // snapshot on disk, and every restored tab would open empty and
        // re-query.
        assert_eq!(
            object_grid_key("public", "accounts", ""),
            "o-public.accounts"
        );
        assert_ne!(
            object_grid_key("public", "accounts", r#""id" = '42'"#),
            object_grid_key("public", "accounts", "")
        );
    }

    #[test]
    fn a_filter_cannot_collide_a_grid_key_with_another_tab() {
        // The separator-escaping property above, extended to the new component.
        assert_ne!(
            object_grid_key("a", "b", "c"),
            object_grid_key("a", "b.c", "")
        );
        assert_ne!(
            object_grid_key("a", "b", "c"),
            object_grid_key("a.b", "c", "")
        );
    }
}
