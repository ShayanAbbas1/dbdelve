use std::time::Duration;

use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/ShayanAbbas1/dbdelve/releases/latest";

#[derive(Clone, Debug)]
pub(crate) struct Release {
    pub(crate) version: String,
    pub(crate) url: String,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
}

/// The latest release, if it is newer than this build. Blocking, so it belongs
/// on the background executor. Any failure is `None`: being offline is not
/// something to tell anyone about.
pub(crate) fn newer_release() -> Option<Release> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(concat!("dbdelve/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let latest: Latest = agent
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    let version = latest.tag_name.trim_start_matches('v').to_string();
    is_newer(&version, env!("CARGO_PKG_VERSION")).then_some(Release {
        version,
        url: latest.html_url,
    })
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
    use super::is_newer;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.3.0-beta", "0.2.0"));
    }
}
