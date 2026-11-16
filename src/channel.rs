//! Which release line this binary was built for, fixed at compile time by
//! `DBDELVE_CHANNEL`. Stable and Nightly install side by side, so everything
//! that names the app on disk, in the keychain or to an installer comes from
//! here.

/// A release line. Its names are what an install of it is found by, so
/// Stable's must never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channel {
    Stable,
    Nightly,
}

/// The app's version: `DBDELVE_VERSION` when the build sets it, which only a
/// nightly does, else the crate's.
pub(crate) const VERSION: &str = match option_env!("DBDELVE_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

impl Channel {
    /// The channel this binary was built for. `dev/bundle.sh` passes `release`
    /// and `dev` through the same variable, and both mean Stable.
    pub(crate) fn current() -> Self {
        Self::named(option_env!("DBDELVE_CHANNEL"))
    }

    fn named(name: Option<&str>) -> Self {
        match name {
            Some("nightly") => Self::Nightly,
            _ => Self::Stable,
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Stable => "DBDelve",
            Self::Nightly => "DBDelve Nightly",
        }
    }

    /// The data directory, keychain service, log directory, Linux binary,
    /// desktop entry and Wayland app id, and the start of every release asset
    /// but the macOS disk image.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Stable => "dbdelve",
            Self::Nightly => "dbdelve-nightly",
        }
    }

    /// The bundle's `CFBundleIdentifier`, which the Keychain scopes saved
    /// passwords to.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn bundle_identifier(self) -> &'static str {
        match self {
            Self::Stable => "com.shayanabbas.dbdelve",
            Self::Nightly => "com.shayanabbas.dbdelve.nightly",
        }
    }

    /// The bundle's executable, and its name without `.app`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn bundle_executable(self) -> &'static str {
        self.display_name()
    }

    /// The app as the disk image holds it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn app_bundle(self) -> String {
        format!("{}.app", self.bundle_executable())
    }

    /// The macOS release asset for `version`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn disk_image(self, version: &str) -> String {
        match self {
            Self::Stable => format!("DBDelve-{version}.dmg"),
            Self::Nightly => format!("DBDelve-Nightly-{version}.dmg"),
        }
    }

    /// The Inno Setup AppId, which names the install's Uninstall entry.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn windows_app_id(self) -> &'static str {
        match self {
            Self::Stable => "{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}",
            Self::Nightly => "{221ADDEE-C041-4E6A-A2E9-72E92DF71DCF}",
        }
    }
}

/// What this app sends as its HTTP user agent.
pub(crate) fn user_agent() -> String {
    format!("dbdelve/{VERSION}")
}

#[cfg(test)]
mod tests {
    use super::{Channel, VERSION};

    #[test]
    fn only_nightly_names_the_nightly_channel() {
        assert_eq!(Channel::named(Some("nightly")), Channel::Nightly);
        for other in [
            None,
            Some(""),
            Some("release"),
            Some("dev"),
            Some("Nightly"),
        ] {
            assert_eq!(Channel::named(other), Channel::Stable, "{other:?}");
        }
    }

    #[test]
    fn a_default_build_is_stable_at_the_crate_version() {
        assert_eq!(Channel::named(None), Channel::Stable);
        if option_env!("DBDELVE_VERSION").is_none_or(str::is_empty) {
            assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        }
    }

    /// Stable's names are what existing installs are found by.
    #[test]
    fn stable_keeps_every_name_it_shipped_with() {
        let stable = Channel::Stable;
        assert_eq!(stable.display_name(), "DBDelve");
        assert_eq!(stable.slug(), "dbdelve");
        assert_eq!(stable.bundle_identifier(), "com.shayanabbas.dbdelve");
        assert_eq!(stable.bundle_executable(), "DBDelve");
        assert_eq!(stable.app_bundle(), "DBDelve.app");
        assert_eq!(stable.disk_image("0.8.0"), "DBDelve-0.8.0.dmg");
        assert_eq!(
            stable.windows_app_id(),
            "{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}"
        );
    }

    #[test]
    fn nightly_is_named_apart_from_stable() {
        let nightly = Channel::Nightly;
        assert_eq!(nightly.display_name(), "DBDelve Nightly");
        assert_eq!(nightly.slug(), "dbdelve-nightly");
        assert_eq!(
            nightly.bundle_identifier(),
            "com.shayanabbas.dbdelve.nightly"
        );
        assert_eq!(nightly.bundle_executable(), "DBDelve Nightly");
        assert_eq!(nightly.app_bundle(), "DBDelve Nightly.app");
        assert_eq!(
            nightly.disk_image("0.7.4-nightly.20261116.12"),
            "DBDelve-Nightly-0.7.4-nightly.20261116.12.dmg"
        );
        assert_eq!(
            nightly.windows_app_id(),
            "{221ADDEE-C041-4E6A-A2E9-72E92DF71DCF}"
        );
    }
}
