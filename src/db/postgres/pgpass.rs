//! libpq's password file, which tokio-postgres does not read: `PGPASSFILE`,
//! else `~/.pgpass` (`%APPDATA%\postgresql\pgpass.conf` on Windows). Read on
//! every connect rather than once, so a file a rotation job rewrites is what
//! the next session signs in with.

use std::io::Read as _;
use std::path::PathBuf;

pub(super) enum Lookup {
    Found {
        password: String,
        path: PathBuf,
    },
    /// libpq refuses a file the group or others can read, and so does dbdelve.
    #[cfg(unix)]
    Exposed(PathBuf),
    Missing,
}

impl Lookup {
    /// What the user cannot otherwise tell about a sign-in that failed: which
    /// file the rejected password came from, or that the file was passed over.
    pub(super) fn note(&self, error: &tokio_postgres::Error) -> Option<String> {
        let rejected = error.code() == Some(&tokio_postgres::error::SqlState::INVALID_PASSWORD);
        match self {
            Self::Found { path, .. } if rejected => {
                Some(format!("The password came from {}.", path.display()))
            }
            #[cfg(unix)]
            Self::Exposed(path) if rejected || password_missing(error) => Some(format!(
                "{} was not read: it is readable by other users, and Postgres clients only \
                     read it at 0600 or stricter.",
                path.display()
            )),
            _ => None,
        }
    }
}

/// tokio-postgres has no error kind for this one, only the words, and they
/// sit on the source rather than in its own message.
#[cfg(unix)]
fn password_missing(error: &tokio_postgres::Error) -> bool {
    std::error::Error::source(error).is_some_and(|source| source.to_string() == "password missing")
}

pub(super) fn lookup(host: &str, port: u16, database: &str, user: &str) -> Lookup {
    let Some(path) = path() else {
        return Lookup::Missing;
    };
    // One handle for the check and the read, so a rotation swapping the file
    // in between cannot pass one file's permissions off as another's.
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Lookup::Missing;
    };
    let Ok(metadata) = file.metadata() else {
        return Lookup::Missing;
    };
    if !metadata.is_file() {
        return Lookup::Missing;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Lookup::Exposed(path);
        }
    }
    let mut contents = Vec::new();
    if file.read_to_end(&mut contents).is_err() {
        return Lookup::Missing;
    }
    // libpq reads bytes, so a stray byte in a comment or another line must not
    // cost every line its use. A field holding one matches nothing dbdelve
    // asks for, which leaves the first-match order as libpq has it.
    let contents = String::from_utf8_lossy(&contents);
    match password(&contents, [host, &port.to_string(), database, user]) {
        Some(password) => Lookup::Found { password, path },
        None => Lookup::Missing,
    }
}

fn path() -> Option<PathBuf> {
    if let Some(file) = std::env::var_os("PGPASSFILE").filter(|file| !file.is_empty()) {
        return Some(PathBuf::from(file));
    }
    #[cfg(target_os = "windows")]
    {
        crate::store::data_root()
            .ok()
            .map(|root| root.join("postgresql").join("pgpass.conf"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        crate::store::home().ok().map(|home| home.join(".pgpass"))
    }
}

/// The password on the first line whose host, port, database and user all
/// match, `*` matching anything. An empty one is no password, as in libpq,
/// and it still ends the search: a later wildcard line is not consulted.
fn password(contents: &str, wanted: [&str; 4]) -> Option<String> {
    contents
        .lines()
        .find_map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.starts_with('#') {
                return None;
            }
            let mut rest = line;
            for wanted in wanted {
                rest = matched(rest, wanted)?;
            }
            Some(unescaped(rest))
        })
        .filter(|password| !password.is_empty())
}

/// The line after its first field, if that field is `*` or `wanted` once its
/// `\:` and `\\` escapes are undone.
fn matched<'a>(line: &'a str, wanted: &str) -> Option<&'a str> {
    if let Some(rest) = line.strip_prefix("*:") {
        return Some(rest);
    }
    let mut wanted = wanted.chars();
    let mut characters = line.char_indices();
    loop {
        let character = match characters.next()? {
            (index, ':') => return wanted.next().is_none().then(|| &line[index + 1..]),
            (_, '\\') => characters.next()?.1,
            (_, character) => character,
        };
        if wanted.next() != Some(character) {
            return None;
        }
    }
}

/// libpq ends the password at the first unescaped `:`, so a trailing field
/// some tool appended is not sent along with it.
fn unescaped(field: &str) -> String {
    let mut password = String::new();
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        match character {
            ':' => break,
            '\\' => match characters.next() {
                Some(escaped) => password.push(escaped),
                None => password.push('\\'),
            },
            character => password.push(character),
        }
    }
    password
}

#[cfg(test)]
mod tests {
    use super::password;

    const WANTED: [&str; 4] = ["db.internal", "5432", "orders", "svc_orders"];

    #[test]
    fn the_first_matching_line_wins() {
        let contents = "\
db.internal:5432:billing:svc_billing:billing-secret
db.internal:5432:orders:svc_orders:orders-secret
*:*:*:svc_orders:fallback
";
        assert_eq!(password(contents, WANTED).as_deref(), Some("orders-secret"));
    }

    #[test]
    fn a_star_matches_any_value_of_its_field() {
        assert_eq!(
            password("*:*:*:svc_orders:wild", WANTED).as_deref(),
            Some("wild")
        );
        assert_eq!(password("*:*:*:someone_else:wild", WANTED), None);
    }

    #[test]
    fn every_field_has_to_match() {
        for line in [
            "db.other:5432:orders:svc_orders:x",
            "db.internal:6432:orders:svc_orders:x",
            "db.internal:5432:payments:svc_orders:x",
            "db.internal:5432:orders:svc_payments:x",
            // A prefix is not a match.
            "db.internal:5432:order:svc_orders:x",
        ] {
            assert_eq!(password(line, WANTED), None, "{line}");
        }
    }

    #[test]
    fn comments_blank_lines_and_crlf_are_handled() {
        let contents = "# rotated nightly\r\n\r\ndb.internal:5432:orders:svc_orders:s3cret\r\n";
        assert_eq!(password(contents, WANTED).as_deref(), Some("s3cret"));
    }

    #[test]
    fn escapes_are_undone_in_fields_and_the_password() {
        let wanted = ["host:with:colons", "5432", r"back\slash", "svc_orders"];
        assert_eq!(
            password(
                r"host\:with\:colons:5432:back\\slash:svc_orders:pa\:ss\\word",
                wanted
            )
            .as_deref(),
            Some(r"pa:ss\word")
        );
    }

    #[test]
    fn the_password_stops_at_an_unescaped_colon() {
        assert_eq!(
            password("*:*:*:*:secret:trailing", WANTED).as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn a_line_that_is_not_utf8_leaves_the_others_usable() {
        let bytes = b"# caf\xe9\n*:*:*:svc_orders:s3cret\n";
        assert_eq!(
            password(&String::from_utf8_lossy(bytes), WANTED).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn an_empty_password_is_none() {
        assert_eq!(password("*:*:*:*:", WANTED), None);
        assert_eq!(
            password(
                "db.internal:5432:orders:svc_orders:\n*:*:*:*:wildcard",
                WANTED
            ),
            None
        );
        // A line with too few fields offers nothing.
        assert_eq!(password("*:*:*", WANTED), None);
    }
}
