//! The driver settings a profile may set, a closed list per engine.
//!
//! Each engine module maps a [`Property`] to the one driver field it names. There
//! is no passthrough of free keys: a free key could carry the statement timeout
//! (Postgres `options`, MySQL `init`), TLS settings or credentials, and every
//! one of those is set from a field that has its own gate.

use super::Engine;

/// A driver setting any engine may list in [`EngineSpec::driver_properties`].
///
/// [`EngineSpec::driver_properties`]: super::spec::EngineSpec::driver_properties
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    /// The name the server shows for this client.
    ApplicationName,
    /// How long dialing the server may take, in whole seconds.
    ConnectTimeout,
}

impl Property {
    /// The spelling in `profiles.toml`, matched exactly.
    pub fn key(self) -> &'static str {
        match self {
            Self::ApplicationName => "application_name",
            Self::ConnectTimeout => "connect_timeout",
        }
    }
}

/// The longest an application name may be. Postgres truncates at 63 bytes and
/// shows the rest nowhere, so a longer one only misleads.
const APPLICATION_NAME_CAP: usize = 64;

/// What a profile's driver properties say, typed. Part of the connection: a
/// change reconnects, and every session opened from it carries them as it
/// carries the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Properties {
    pub application_name: Option<String>,
    /// Whole seconds, 1 to 600.
    pub connect_timeout: Option<u32>,
}

impl Properties {
    /// What `pairs` set on `engine`, or why they cannot. A key outside the
    /// engine's list is refused by name, and the whole list with it, so a
    /// profile never connects with part of what was written.
    pub fn parse(engine: Engine, pairs: &[(String, String)]) -> Result<Self, String> {
        let allowed = engine.spec().driver_properties;
        let mut properties = Self::default();
        let mut seen: Vec<Property> = Vec::new();
        for (key, value) in pairs {
            let Some(property) = allowed
                .iter()
                .copied()
                .find(|property| property.key() == key)
            else {
                return Err(format!(
                    "Unknown driver property `{key}` for {}.",
                    engine.label()
                ));
            };
            if seen.contains(&property) {
                return Err(format!("Driver property `{key}` is given twice."));
            }
            seen.push(property);
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("Driver property `{key}` has no value."));
            }
            match property {
                Property::ApplicationName => {
                    if value.chars().count() > APPLICATION_NAME_CAP {
                        return Err(format!(
                            "Driver property `{key}` is longer than {APPLICATION_NAME_CAP} characters."
                        ));
                    }
                    if value.chars().any(char::is_control) {
                        return Err(format!(
                            "Driver property `{key}` holds a control character."
                        ));
                    }
                    properties.application_name = Some(value.to_string());
                }
                Property::ConnectTimeout => {
                    let seconds = value
                        .parse::<u32>()
                        .ok()
                        .filter(|seconds| (1..=600).contains(seconds))
                        .ok_or_else(|| {
                            format!(
                                "Driver property `{key}` is `{value}`, not a whole number of \
                                 seconds from 1 to 600."
                            )
                        })?;
                    properties.connect_timeout = Some(seconds);
                }
            }
        }
        Ok(properties)
    }

    /// What `text` sets on `engine`: one `key = value` per line, blank lines
    /// skipped, split at the first `=` so a value may hold one. A line with no
    /// `=` is refused by its number, then the pairs go through [`Self::parse`].
    pub fn from_lines(engine: Engine, text: &str) -> Result<Self, String> {
        let mut pairs = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("Driver property line {} has no `=`.", index + 1));
            };
            pairs.push((key.trim().to_string(), value.trim().to_string()));
        }
        Self::parse(engine, &pairs)
    }

    /// The set values as [`Self::from_lines`] reads them, in [`Self::stored`]'s
    /// order.
    pub fn lines(&self) -> String {
        self.stored()
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The set values as `profiles.toml` writes them, in the order an engine's
    /// list gives, whatever order they were written in, so saving an unchanged
    /// profile changes no byte.
    pub fn stored(&self) -> Vec<(String, String)> {
        [
            self.application_name
                .as_ref()
                .map(|name| (Property::ApplicationName, name.clone())),
            self.connect_timeout
                .map(|seconds| (Property::ConnectTimeout, seconds.to_string())),
        ]
        .into_iter()
        .flatten()
        .map(|(property, value)| (property.key().to_string(), value))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(key: &str, value: &str) -> Vec<(String, String)> {
        vec![(key.to_string(), value.to_string())]
    }

    #[test]
    fn an_unknown_driver_property_is_refused_by_name() {
        let refused = |engine: Engine, key: &str, value: &str| {
            Properties::parse(engine, &pair(key, value)).expect_err(&format!("{engine:?} {key}"))
        };
        let message = refused(Engine::Postgres, "foo", "1");
        assert!(
            message.contains("`foo`") && message.contains("Postgres"),
            "{message}"
        );
        assert!(refused(Engine::Postgres, "Application_Name", "x").contains("`Application_Name`"));
        assert!(refused(Engine::SqlServer, "connect_timeout", "5").contains("`connect_timeout`"));
        for engine in [Engine::Redis, Engine::Sqlite, Engine::Snowflake] {
            assert!(refused(engine, "application_name", "x").contains("`application_name`"));
        }
        assert!(
            refused(
                Engine::Postgres,
                "options",
                "-c default_transaction_read_only=off"
            )
            .contains("`options`")
        );
        assert!(refused(Engine::Postgres, "sslmode", "disable").contains("`sslmode`"));
    }

    #[test]
    fn no_allowed_property_touches_the_hold_tls_or_credentials() {
        const DENIED: [&str; 24] = [
            "options",
            "init",
            "sslmode",
            "sslrootcert",
            "sslcert",
            "sslkey",
            "ssl_opts",
            "channel_binding",
            "password",
            "pass",
            "user",
            "host",
            "hostaddr",
            "port",
            "dbname",
            "service",
            "replication",
            "target_session_attrs",
            "local_infile",
            "prefer_socket",
            "trust_cert",
            "encryption",
            "authentication",
            "readonly",
        ];
        for engine in Engine::ALL {
            let allowed = engine.spec().driver_properties;
            for property in allowed {
                assert!(!DENIED.contains(&property.key()), "{engine:?} {property:?}");
            }
            let expected: &[Property] = match engine {
                Engine::Postgres | Engine::MySql | Engine::MariaDb => {
                    &[Property::ApplicationName, Property::ConnectTimeout]
                }
                Engine::SqlServer => &[Property::ApplicationName],
                _ => &[],
            };
            assert_eq!(allowed, expected, "{engine:?}");
        }
    }

    #[test]
    fn values_are_typed_and_stored_in_list_order() {
        for bad in ["0", "601", "ten", "-1", ""] {
            let message =
                Properties::parse(Engine::Postgres, &pair("connect_timeout", bad)).expect_err(bad);
            assert!(message.contains("`connect_timeout`"), "{bad}: {message}");
        }
        for good in [("1", 1), ("600", 600)] {
            let parsed =
                Properties::parse(Engine::MySql, &pair("connect_timeout", good.0)).expect(good.0);
            assert_eq!(parsed.connect_timeout, Some(good.1));
        }
        let long = "a".repeat(65);
        for bad in [long.as_str(), "a\nb"] {
            let message =
                Properties::parse(Engine::Postgres, &pair("application_name", bad)).expect_err(bad);
            assert!(message.contains("`application_name`"), "{message}");
        }
        assert!(
            Properties::parse(Engine::Postgres, &pair("application_name", &"a".repeat(64))).is_ok()
        );

        let twice = vec![
            ("application_name".to_string(), "a".to_string()),
            ("application_name".to_string(), "b".to_string()),
        ];
        let message = Properties::parse(Engine::Postgres, &twice).expect_err("a key twice");
        assert!(
            message.contains("`application_name`") && message.contains("twice"),
            "{message}"
        );

        let backwards = vec![
            ("connect_timeout".to_string(), "5".to_string()),
            ("application_name".to_string(), "billing".to_string()),
        ];
        let parsed = Properties::parse(Engine::Postgres, &backwards).expect("both are listed");
        let keys: Vec<_> = parsed.stored().into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["application_name", "connect_timeout"]);
        assert_eq!(Properties::default().stored(), []);
    }

    #[test]
    fn lines_are_read_and_written_back() {
        let text = "\n  connect_timeout =  5 \n\napplication_name=o'brien = app\n";
        let parsed = Properties::from_lines(Engine::Postgres, text).expect("both are listed");
        assert_eq!(parsed.application_name.as_deref(), Some("o'brien = app"));
        assert_eq!(parsed.connect_timeout, Some(5));
        let written = parsed.lines();
        assert_eq!(
            written,
            "application_name = o'brien = app\nconnect_timeout = 5"
        );
        assert_eq!(
            Properties::from_lines(Engine::Postgres, &written),
            Ok(parsed)
        );
        assert_eq!(Properties::default().lines(), "");
        assert_eq!(
            Properties::from_lines(Engine::Postgres, "  \n\n"),
            Ok(Properties::default())
        );
        let message = Properties::from_lines(Engine::Postgres, "connect_timeout = 5\n\nfoo")
            .expect_err("no `=`");
        assert!(message.contains("line 3"), "{message}");
    }
}
