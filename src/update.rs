// macOS, Linux and Windows replace themselves. Elsewhere the install half
// still compiles, so the workspace drives one state machine on every platform,
// and goes unused.
#![cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::Deserialize;

use crate::channel::{self, Channel};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod download;
#[cfg(any(target_os = "linux", windows, all(test, target_os = "macos")))]
mod identity;
// Also built into the macOS tests, which are the ones a Mac can run.
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, all(test, target_os = "macos")))]
#[cfg_attr(not(windows), allow(dead_code))]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::{Installer, install};
#[cfg(target_os = "macos")]
pub(crate) use macos::{Installer, install};
#[cfg(windows)]
pub(crate) use windows::{Installer, install};

const RELEASES: &str = "https://api.github.com/repos/ShayanAbbas1/dbdelve/releases";

const CURRENT: &str = channel::VERSION;

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
    Ready(Arc<Installer>),
    Running(Arc<Progress>),
    /// Why it stopped, in words for the dropdown; the detail is in the log.
    /// The installer is kept so the same update can be tried again.
    Failed(&'static str, Arc<Installer>),
    /// Another DBDelve has replaced this install since it started, so trying
    /// again cannot succeed; restarting into the install at this path runs
    /// what it put there.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    Superseded(PathBuf),
}

impl State {
    /// What an install that stopped for `reason` leaves.
    pub(crate) fn stopped(reason: &'static str, installer: Arc<Installer>) -> Self {
        #[cfg(any(target_os = "linux", windows))]
        if reason == *identity::ALREADY_UPDATED {
            return State::Superseded(installer.installed());
        }
        State::Failed(reason, installer)
    }
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

/// An update file and its minisign signature, as the release lists them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Asset {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) size: u64,
    pub(crate) signature_url: String,
}

/// The latest release of this build's channel, if it is newer than this
/// build, and whether this install can put it in place itself. Blocking, so
/// it belongs on the background executor. Any failure is `None`: being
/// offline is not something to tell anyone about.
pub(crate) fn newer_release() -> Option<Update> {
    let channel = Channel::current();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(channel::user_agent())
        .build()
        .into();
    let feed = feed(channel);
    let response = agent
        .get(&feed)
        .header("Accept", "application/vnd.github+json")
        .call();
    let latest = match channel {
        Channel::Stable => response
            .and_then(|mut response| response.body_mut().read_json::<Latest>())
            .map(Some),
        Channel::Nightly => response
            .and_then(|mut response| response.body_mut().read_json::<Vec<Latest>>())
            .map(|releases| newest(channel, releases)),
    };
    let latest = match latest {
        Ok(Some(latest)) => latest,
        Ok(None) => {
            tracing::info!(feed, "the feed lists no release of this channel");
            return None;
        }
        Err(error) => {
            tracing::info!(feed, "update check failed: {error}");
            return None;
        }
    };
    let version = tag_version(&latest.tag_name).to_string();
    if !is_newer_in(channel, &version, CURRENT) {
        return None;
    }
    tracing::info!(version, current = CURRENT, "a newer release is out");
    Some(Update {
        state: installer(channel, &version, &latest.assets)
            .map_or(State::Link, |installer| State::Ready(Arc::new(installer))),
        release: Release {
            version,
            url: latest.html_url,
        },
    })
}

/// The highest release of `channel` among `releases`, whatever order the feed
/// lists them in.
fn newest(channel: Channel, releases: Vec<Latest>) -> Option<Latest> {
    releases
        .into_iter()
        .filter_map(|release| Some((Version::parse(tag_version(&release.tag_name))?, release)))
        .filter(|(version, _)| version.channel() == channel)
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
}

fn tag_version(tag: &str) -> &str {
    tag.trim_start_matches('v')
}

/// Just ahead of `App::restart` into an install.
pub(crate) fn before_restart() {
    #[cfg(target_os = "linux")]
    linux::close_inherited_files();
}

/// Notes what this process was installed as, before any update check can find
/// that another process has replaced it since. Once, at startup.
pub(crate) fn remember_install() {
    #[cfg(target_os = "linux")]
    linux::remember_install();
    #[cfg(windows)]
    windows::remember_install();
}

/// Only a test build reads its feed from the environment, so a shipped one
/// cannot be pointed anywhere but the real releases. Stable's feed is the one
/// latest release, which GitHub never makes a pre-release; Nightly's is a list,
/// since every nightly is one.
fn feed(channel: Channel) -> String {
    #[cfg(feature = "update-test-hooks")]
    if let Some(feed) = std::env::var("DBDELVE_UPDATE_FEED")
        .ok()
        .filter(|feed| !feed.is_empty())
    {
        return feed;
    }
    match channel {
        Channel::Stable => format!("{RELEASES}/latest"),
        Channel::Nightly => format!("{RELEASES}?per_page=30"),
    }
}

/// Whether a found update installs without anyone clicking: only a test build
/// told to by `DBDELVE_UPDATE_AUTO=1`, for an end-to-end test of the updater
/// that has nobody at the screen.
pub(crate) fn installs_unattended() -> bool {
    #[cfg(feature = "update-test-hooks")]
    if std::env::var("DBDELVE_UPDATE_AUTO").is_ok_and(|value| value == "1") {
        return true;
    }
    false
}

/// Exactly the asset called `name` and its `.minisig`, or nothing: an update
/// without its signature is not one this app will install.
fn asset(name: &str, assets: &[LatestAsset]) -> Option<Asset> {
    let signature = format!("{name}.minisig");
    let find = |wanted: &str| assets.iter().find(|asset| asset.name == wanted);
    let (asset, signature) = (find(name)?, find(&signature)?);
    Some(Asset {
        name: asset.name.clone(),
        url: asset.browser_download_url.clone(),
        size: asset.size,
        signature_url: signature.browser_download_url.clone(),
    })
}

#[cfg(target_os = "macos")]
fn installer(channel: Channel, version: &str, assets: &[LatestAsset]) -> Option<Installer> {
    macos::installer(version, asset(&channel.disk_image(version), assets))
}

#[cfg(target_os = "linux")]
fn installer(channel: Channel, version: &str, assets: &[LatestAsset]) -> Option<Installer> {
    linux::installer(channel, version, |name| asset(name, assets))
}

#[cfg(windows)]
fn installer(channel: Channel, version: &str, assets: &[LatestAsset]) -> Option<Installer> {
    windows::installer(channel, version, |name| asset(name, assets))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) enum Installer {}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn installer(_: Channel, _: &str, _: &[LatestAsset]) -> Option<Installer> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn install(installer: &Installer, _: &Progress) -> Result<PathBuf, &'static str> {
    match *installer {}
}

/// A release's version: `X.Y.Z` on Stable, `X.Y.Z-nightly.YYYYMMDD.RUN` on
/// Nightly. Ordered numerically, field by field, so 0.2.10 beats 0.2.9 and a
/// nightly's date and run come after its `X.Y.Z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    nightly: Option<NightlyBuild>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NightlyBuild {
    date: u64,
    run: u64,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let (core, nightly) = match text.split_once('-') {
            Some((core, build)) => (core, Some(build.strip_prefix("nightly.")?)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut part = || number(parts.next()?);
        let (major, minor, patch) = (part()?, part()?, part()?);
        if parts.next().is_some() {
            return None;
        }
        let nightly = match nightly {
            Some(build) => {
                let (date, run) = build.split_once('.')?;
                if date.len() != 8 {
                    return None;
                }
                Some(NightlyBuild {
                    date: number(date)?,
                    run: number(run)?,
                })
            }
            None => None,
        };
        Some(Self {
            major,
            minor,
            patch,
            nightly,
        })
    }

    fn channel(self) -> Channel {
        match self.nightly {
            Some(_) => Channel::Nightly,
            None => Channel::Stable,
        }
    }
}

/// Digits only: `parse` alone would take a sign.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Whether `candidate` is an update for a build at `current`. A build ahead of
/// the latest release (a local one, say) is not told to "update" backwards.
#[cfg(any(target_os = "macos", test))]
fn is_newer(candidate: &str, current: &str) -> bool {
    is_newer_in(Channel::current(), candidate, current)
}

/// Only a release of `channel` is one, so Nightly is never offered Stable and
/// Stable never a nightly.
fn is_newer_in(channel: Channel, candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(candidate), Some(current)) => candidate.channel() == channel && candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Asset, Latest, Progress, Version, asset, is_newer, is_newer_in, newest};
    use crate::channel::Channel;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.3.0-beta", "0.2.0"));
    }

    #[test]
    fn reads_only_a_stable_or_a_nightly_version() {
        let nightly = Version::parse("0.7.4-nightly.20261116.12").unwrap();
        assert_eq!(nightly.channel(), Channel::Nightly);
        assert_eq!(Version::parse("0.7.3").unwrap().channel(), Channel::Stable);
        for garbage in [
            "",
            "0.7",
            "0.7.3.1",
            "0.7.x",
            "+0.7.3",
            "0.7.3-",
            "0.7.3-beta",
            "0.7.3-beta.20261116.1",
            "0.7.4-nightly",
            "0.7.4-nightly.20261116",
            "0.7.4-nightly.2026111.1",
            "0.7.4-nightly.202611160.1",
            "0.7.4-nightly.20261116.",
            "0.7.4-nightly.20261116.1.2",
            "0.7.4-nightly.20261116.+1",
            "0.7.4-nightly.2026-11-16.1",
            "v0.7.3",
        ] {
            assert_eq!(Version::parse(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn orders_nightlies_by_version_then_date_then_run() {
        let nightly = Channel::Nightly;
        let current = "0.7.4-nightly.20261116.12";
        for newer in [
            "0.7.4-nightly.20261116.13",
            "0.7.4-nightly.20261116.100",
            "0.7.4-nightly.20261117.1",
            "0.7.5-nightly.20261101.1",
            "0.8.0-nightly.20261116.1",
        ] {
            assert!(is_newer_in(nightly, newer, current), "{newer}");
        }
        for older in [
            current,
            "0.7.4-nightly.20261116.11",
            "0.7.4-nightly.20261115.99",
            "0.7.3-nightly.20261201.1",
        ] {
            assert!(!is_newer_in(nightly, older, current), "{older}");
        }
        // A nightly built without a nightly version, locally say, still hears
        // about the nightlies after it.
        assert!(is_newer_in(nightly, "0.7.4-nightly.20261116.1", "0.7.3"));
        assert!(!is_newer_in(nightly, "garbage", current));
        assert!(!is_newer_in(nightly, current, "garbage"));
    }

    #[test]
    fn a_channel_is_offered_only_its_own_releases() {
        assert!(!is_newer_in(
            Channel::Nightly,
            "9.0.0",
            "0.7.4-nightly.20261116.12"
        ));
        assert!(!is_newer_in(
            Channel::Stable,
            "0.7.4-nightly.20261116.12",
            "0.7.3"
        ));
        assert!(is_newer_in(Channel::Stable, "0.7.4", "0.7.3"));
    }

    /// Trimmed from a real `releases` listing: newest first, stable releases
    /// among the nightlies, and one tag that is neither.
    const RELEASES: &str = r#"[
        {"tag_name": "v0.7.5-nightly.20261118.15",
         "html_url": "https://example.test/v0.7.5-nightly.20261118.15"},
        {"tag_name": "v0.8.0", "html_url": "https://example.test/v0.8.0"},
        {"tag_name": "v0.7.5-nightly.20261117.20",
         "html_url": "https://example.test/v0.7.5-nightly.20261117.20"},
        {"tag_name": "v0.7.5-nightly.20261118.9",
         "html_url": "https://example.test/v0.7.5-nightly.20261118.9"},
        {"tag_name": "v9.9.9-rc1", "html_url": "https://example.test/v9.9.9-rc1"},
        {"tag_name": "v0.7.4", "html_url": "https://example.test/v0.7.4"}
    ]"#;

    #[test]
    fn the_newest_nightly_is_picked_from_the_listing() {
        let releases = || serde_json::from_str::<Vec<Latest>>(RELEASES).unwrap();
        let picked = newest(Channel::Nightly, releases()).unwrap();
        assert_eq!(picked.tag_name, "v0.7.5-nightly.20261118.15");
        assert_eq!(
            picked.html_url,
            "https://example.test/v0.7.5-nightly.20261118.15"
        );
        assert!(picked.assets.is_empty());

        assert_eq!(
            newest(Channel::Stable, releases()).unwrap().tag_name,
            "v0.8.0"
        );
        assert!(newest(Channel::Nightly, Vec::new()).is_none());
        let stable_only = releases()
            .into_iter()
            .filter(|release| !release.tag_name.contains("nightly"))
            .collect();
        assert!(newest(Channel::Nightly, stable_only).is_none());
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
            {"name": "dbdelve-0.8.0-linux-x86_64.tar.gz.minisig", "size": 310,
             "browser_download_url": "https://example.test/dbdelve-0.8.0-linux-x86_64.tar.gz.minisig"},
            {"name": "dbdelve-0.8.0-x86_64.AppImage", "size": 3,
             "browser_download_url": "https://example.test/dbdelve-0.8.0-x86_64.AppImage"},
            {"name": "dbdelve-0.8.0-x86_64.AppImage.minisig", "size": 310,
             "browser_download_url": "https://example.test/dbdelve-0.8.0-x86_64.AppImage.minisig"},
            {"name": "dbdelve-0.8.0-aarch64.AppImage", "size": 4,
             "browser_download_url": "https://example.test/dbdelve-0.8.0-aarch64.AppImage"},
            {"name": "DBDelve-0.8.0.dmg", "size": 41943040,
             "browser_download_url": "https://example.test/DBDelve-0.8.0.dmg"}
        ]
    }"#;

    #[test]
    fn picks_the_named_asset_and_its_signature() {
        let latest: Latest = serde_json::from_str(LATEST).unwrap();
        let listed = |name: &str, size: u64| Asset {
            name: name.into(),
            url: format!("https://example.test/{name}"),
            size,
            signature_url: format!("https://example.test/{name}.minisig"),
        };
        for (name, size) in [
            ("DBDelve-0.8.0.dmg", 41943040),
            ("dbdelve-0.8.0-linux-x86_64.tar.gz", 2),
            ("dbdelve-0.8.0-x86_64.AppImage", 3),
        ] {
            assert_eq!(asset(name, &latest.assets), Some(listed(name, size)));
        }
        // Another version's dmg is not this release's update.
        assert_eq!(asset("DBDelve-0.7.9.dmg", &latest.assets), None);
        // Nor is an asset the release does not have.
        assert_eq!(
            asset("dbdelve-0.8.0-linux-aarch64.tar.gz", &latest.assets),
            None
        );
    }

    #[test]
    fn an_asset_without_its_signature_is_not_an_update() {
        let latest: Latest = serde_json::from_str(LATEST).unwrap();
        // Listed, but its .minisig is not.
        assert_eq!(
            asset("dbdelve-0.8.0-aarch64.AppImage", &latest.assets),
            None
        );

        let mut unsigned: Latest = serde_json::from_str(LATEST).unwrap();
        unsigned
            .assets
            .retain(|asset| !asset.name.ends_with(".minisig"));
        for name in [
            "DBDelve-0.8.0.dmg",
            "dbdelve-0.8.0-linux-x86_64.tar.gz",
            "dbdelve-0.8.0-x86_64.AppImage",
        ] {
            assert_eq!(asset(name, &unsigned.assets), None, "{name}");
        }

        let without_assets: Latest =
            serde_json::from_str(r#"{"tag_name": "v0.8.0", "html_url": "https://example.test"}"#)
                .unwrap();
        assert_eq!(asset("DBDelve-0.8.0.dmg", &without_assets.assets), None);
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
