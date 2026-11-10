// Only macOS replaces itself so far. Elsewhere the install half still
// compiles, so the workspace drives one state machine on every platform, and
// goes unused.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use serde::Deserialize;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(crate) use macos::{Installer, install};

const LATEST: &str = "https://api.github.com/repos/ShayanAbbas1/dbdelve/releases/latest";

const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// How often a running app asks again, so a session left open for days still
/// hears about a release.
pub(crate) const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug)]
pub(crate) struct Release {
    pub(crate) version: String,
    pub(crate) url: String,
}

/// A newer release, and how far this install has got with putting it in place.
pub(crate) struct Update {
    pub(crate) release: Release,
    pub(crate) state: State,
}

pub(crate) enum State {
    /// This install cannot replace itself, so the release page is all there is.
    Link,
    Ready(Installer),
    Running(std::sync::Arc<Progress>),
    /// Why it stopped, in words for the dropdown; the detail is in the log.
    Failed(&'static str),
}

/// Written by the install on its own thread, read by the titlebar on a tick.
#[derive(Default)]
pub(crate) struct Progress {
    received: AtomicU64,
    total: AtomicU64,
    installing: AtomicBool,
}

impl Progress {
    pub(crate) fn downloaded(&self, received: u64, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.received.store(received, Ordering::Relaxed);
    }

    pub(crate) fn installing(&self) {
        self.installing.store(true, Ordering::Relaxed);
    }

    /// What the titlebar pill reads while the update runs.
    pub(crate) fn label(&self) -> String {
        if self.installing.load(Ordering::Relaxed) {
            return "Installing\u{2026}".into();
        }
        let total = self.total.load(Ordering::Relaxed);
        let received = self.received.load(Ordering::Relaxed);
        match (received * 100).checked_div(total) {
            Some(percent) => format!("Updating\u{2026} {}%", percent.min(100)),
            None => "Updating\u{2026}".into(),
        }
    }
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<LatestAsset>,
}

#[derive(Deserialize)]
struct LatestAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// The macOS update and its minisign signature, as the release lists them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Dmg {
    pub(crate) url: String,
    pub(crate) size: u64,
    pub(crate) signature_url: String,
}

/// The latest release, if it is newer than this build, and whether this install
/// can put it in place itself. Blocking, so it belongs on the background
/// executor. Any failure is `None`: being offline is not something to tell
/// anyone about.
pub(crate) fn newer_release() -> Option<Update> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(concat!("dbdelve/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let feed = feed();
    let latest: Latest = match agent
        .get(&feed)
        .header("Accept", "application/vnd.github+json")
        .call()
        .and_then(|mut response| response.body_mut().read_json())
    {
        Ok(latest) => latest,
        Err(error) => {
            tracing::info!(feed, "update check failed: {error}");
            return None;
        }
    };
    let version = latest.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&version, CURRENT) {
        return None;
    }
    tracing::info!(version, current = CURRENT, "a newer release is out");
    let dmg = dmg(&version, &latest.assets);
    Some(Update {
        state: installer(&version, dmg).map_or(State::Link, State::Ready),
        release: Release {
            version,
            url: latest.html_url,
        },
    })
}

/// Only a test build reads its feed from the environment, so a shipped one
/// cannot be pointed anywhere but the real releases.
fn feed() -> String {
    #[cfg(feature = "update-test-hooks")]
    if let Some(feed) = std::env::var("DBDELVE_UPDATE_FEED")
        .ok()
        .filter(|feed| !feed.is_empty())
    {
        return feed;
    }
    LATEST.to_string()
}

/// Exactly `DBDelve-<version>.dmg` and its `.minisig`, or nothing: an update
/// without its signature is not one this app will install.
fn dmg(version: &str, assets: &[LatestAsset]) -> Option<Dmg> {
    let name = format!("DBDelve-{version}.dmg");
    let signature = format!("{name}.minisig");
    let find = |wanted: &str| assets.iter().find(|asset| asset.name == wanted);
    let (dmg, signature) = (find(&name)?, find(&signature)?);
    Some(Dmg {
        url: dmg.browser_download_url.clone(),
        size: dmg.size,
        signature_url: signature.browser_download_url.clone(),
    })
}

#[cfg(target_os = "macos")]
fn installer(version: &str, dmg: Option<Dmg>) -> Option<Installer> {
    macos::installer(version, dmg)
}

#[cfg(not(target_os = "macos"))]
pub(crate) enum Installer {}

#[cfg(not(target_os = "macos"))]
fn installer(_: &str, _: Option<Dmg>) -> Option<Installer> {
    None
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install(
    installer: Installer,
    _: &Progress,
) -> Result<std::path::PathBuf, &'static str> {
    match installer {}
}

/// Numeric, so 0.2.10 beats 0.2.9, and a build ahead of the latest release
/// (a local one, say) is not told to "update" backwards.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |version: &str| {
        version
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    matches!((parse(candidate), parse(current)), (Some(candidate), Some(current)) if candidate > current)
}

#[cfg(test)]
mod tests {
    use super::{Dmg, Latest, Progress, dmg, is_newer};

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.3.0-beta", "0.2.0"));
    }

    /// Trimmed from a real `releases/latest` response.
    const LATEST: &str = r#"{
        "tag_name": "v0.8.0",
        "html_url": "https://github.com/ShayanAbbas1/dbdelve/releases/tag/v0.8.0",
        "assets": [
            {"name": "DBDelve-0.8.0.dmg.minisig", "size": 310,
             "browser_download_url": "https://example.test/DBDelve-0.8.0.dmg.minisig"},
            {"name": "DBDelve-0.7.9.dmg", "size": 1,
             "browser_download_url": "https://example.test/DBDelve-0.7.9.dmg"},
            {"name": "dbdelve-0.8.0-linux-x86_64.tar.gz", "size": 2,
             "browser_download_url": "https://example.test/dbdelve-0.8.0-linux-x86_64.tar.gz"},
            {"name": "DBDelve-0.8.0.dmg", "size": 41943040,
             "browser_download_url": "https://example.test/DBDelve-0.8.0.dmg"}
        ]
    }"#;

    #[test]
    fn picks_the_releases_own_dmg_and_its_signature() {
        let latest: Latest = serde_json::from_str(LATEST).unwrap();
        assert_eq!(
            dmg("0.8.0", &latest.assets),
            Some(Dmg {
                url: "https://example.test/DBDelve-0.8.0.dmg".into(),
                size: 41943040,
                signature_url: "https://example.test/DBDelve-0.8.0.dmg.minisig".into(),
            })
        );
        // Another version's dmg is not this release's update.
        assert_eq!(dmg("0.7.9", &latest.assets), None);
    }

    #[test]
    fn a_dmg_without_its_signature_is_not_an_update() {
        let mut latest: Latest = serde_json::from_str(LATEST).unwrap();
        latest
            .assets
            .retain(|asset| !asset.name.ends_with(".minisig"));
        assert_eq!(dmg("0.8.0", &latest.assets), None);

        let without_assets: Latest =
            serde_json::from_str(r#"{"tag_name": "v0.8.0", "html_url": "https://example.test"}"#)
                .unwrap();
        assert_eq!(dmg("0.8.0", &without_assets.assets), None);
    }

    #[test]
    fn progress_reads_as_a_percentage_then_installing() {
        let progress = Progress::default();
        assert_eq!(progress.label(), "Updating\u{2026}");
        progress.downloaded(21, 50);
        assert_eq!(progress.label(), "Updating\u{2026} 42%");
        progress.installing();
        assert_eq!(progress.label(), "Installing\u{2026}");
    }
}
