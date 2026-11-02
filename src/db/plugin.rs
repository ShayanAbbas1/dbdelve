//! An engine plugin: a separate executable DBDelve starts for each connection
//! made through it, speaking `dbdelve-plugin-protocol` over its stdin and
//! stdout.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct Plugin {
    /// `[a-z0-9-]+`, persisted as `plugin:<id>`.
    pub id: String,
    /// What errors and the connection form call it: "The ODBC plugin stopped".
    pub name: String,
    pub version: String,
    pub executable: PathBuf,
}

const MANIFEST: &str = "dbdelve-plugin.toml";

/// Where a plugins directory sits decides where its executables are: a macOS
/// bundle keeps code out of `Resources`, which a sealed bundle forbids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Plain,
    MacBundleResources,
}

/// `deny_unknown_fields`: a manifest this build cannot read in full must not
/// load, so a newer plugin says it needs a newer DBDelve instead of half-working.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    name: String,
    version: String,
    protocol: u32,
    executable: String,
}

/// Bundled directories come first, so a user directory cannot shadow a bundled plugin.
pub fn discover() -> Vec<&'static Plugin> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        dirs.push((exe_dir.join("plugins"), Layout::Plain));
        // Only a bundle has a Resources directory beside its executable;
        // elsewhere the same path is the executable's sibling, a Downloads
        // folder's, say, and anyone could put plugins there.
        if cfg!(target_os = "macos") {
            dirs.push((
                exe_dir.join("../Resources/plugins"),
                Layout::MacBundleResources,
            ));
        }
    }
    if let Ok(user) = crate::store::dbdelve_directory() {
        dirs.push((user.join("plugins"), Layout::Plain));
    }
    discover_in(&dirs)
}

/// Reads manifests only; the executable is not checked until it is opened.
/// A bad manifest is reported on stderr and skipped, never fatal.
pub fn discover_in(dirs: &[(PathBuf, Layout)]) -> Vec<&'static Plugin> {
    let mut found: Vec<&'static Plugin> = Vec::new();
    for (dir, layout) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut folders: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        folders.sort();
        for folder in folders {
            let manifest = folder.join(MANIFEST);
            if !manifest.is_file() {
                continue;
            }
            match load(&folder, &manifest, *layout, cfg!(windows)) {
                Ok(plugin) if found.iter().any(|known| known.id == plugin.id) => eprintln!(
                    "plugin {}: skipped, id `{}` is already loaded from an earlier directory",
                    manifest.display(),
                    plugin.id
                ),
                // Leaked so a `Target` can stay `Copy`; a handful, once per run.
                Ok(plugin) => found.push(Box::leak(Box::new(plugin))),
                Err(reason) => eprintln!("plugin {}: skipped, {reason}", manifest.display()),
            }
        }
    }
    found
}

fn load(folder: &Path, manifest: &Path, layout: Layout, windows: bool) -> Result<Plugin, String> {
    let text = std::fs::read_to_string(manifest).map_err(|error| error.to_string())?;
    let parsed: Manifest = toml::from_str(&text).map_err(|error| error.to_string())?;
    if parsed.protocol != dbdelve_plugin_protocol::PROTOCOL {
        return Err(format!(
            "it speaks protocol {}, this build speaks {}",
            parsed.protocol,
            dbdelve_plugin_protocol::PROTOCOL
        ));
    }
    let id_is_valid = !parsed.id.is_empty()
        && parsed
            .id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !id_is_valid {
        return Err(format!("id `{}` is not [a-z0-9-]+", parsed.id));
    }
    let escapes = Path::new(&parsed.executable)
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir));
    if parsed.executable.is_empty() || escapes {
        return Err(format!(
            "executable `{}` must be a relative path without `..`",
            parsed.executable
        ));
    }
    let mut file = parsed.executable.clone();
    if windows {
        file.push_str(".exe");
    }
    let executable = match layout {
        Layout::Plain => folder.join(file),
        Layout::MacBundleResources => {
            let name = Path::new(&file)
                .file_name()
                .ok_or("executable has no file name")?
                .to_owned();
            // `folder` is Contents/Resources/plugins/<id>.
            folder.join("../../../Helpers").join(name)
        }
    };
    Ok(Plugin {
        id: parsed.id,
        name: parsed.name,
        version: parsed.version,
        executable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbdelve-plugin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, folder: &str, body: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST), body).unwrap();
    }

    fn manifest(id: &str, extra: &str) -> String {
        format!(
            "id = \"{id}\"\nname = \"N {id}\"\nversion = \"1.0.0\"\nprotocol = 1\nexecutable = \"bin/run\"\n{extra}"
        )
    }

    fn ids(root: &Path) -> Vec<String> {
        discover_in(&[(root.to_path_buf(), Layout::Plain)])
            .iter()
            .map(|plugin| plugin.id.clone())
            .collect()
    }

    #[test]
    fn a_valid_manifest_loads_without_its_binary_existing() {
        let root = scratch("valid");
        write(&root, "odbc", &manifest("odbc", ""));
        let found = discover_in(&[(root.clone(), Layout::Plain)]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "N odbc");
        assert_eq!(found[0].version, "1.0.0");
        assert_eq!(
            found[0].executable,
            root.join("odbc")
                .join(format!("bin/run{}", std::env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn each_refusal_skips_that_plugin_and_keeps_the_rest() {
        let root = scratch("refusals");
        write(&root, "a-good", &manifest("good", ""));
        write(&root, "b-unknown", &manifest("unknown", "icon = \"x\"\n"));
        write(
            &root,
            "c-protocol",
            &manifest("protocol", "").replace("protocol = 1", "protocol = 2"),
        );
        write(&root, "d-id", &manifest("Bad_Id", ""));
        write(&root, "e-empty-id", &manifest("", ""));
        write(
            &root,
            "f-dots",
            &manifest("dots", "").replace("bin/run", "../run"),
        );
        write(
            &root,
            "g-nested-dots",
            &manifest("nested", "").replace("bin/run", "bin/../../run"),
        );
        write(
            &root,
            "h-absolute",
            &manifest("absolute", "").replace("bin/run", "/usr/bin/run"),
        );
        write(&root, "i-last", &manifest("last", ""));
        assert_eq!(ids(&root), ["good", "last"]);
    }

    #[test]
    fn a_duplicate_id_keeps_the_first_directory() {
        let first = scratch("first");
        let second = scratch("second");
        write(&first, "odbc", &manifest("odbc", ""));
        write(&second, "odbc", &manifest("odbc", ""));
        let found = discover_in(&[(first.clone(), Layout::Plain), (second, Layout::Plain)]);
        assert_eq!(found.len(), 1);
        assert!(found[0].executable.starts_with(&first));
    }

    #[test]
    fn exe_is_appended_on_windows_only() {
        let root = scratch("exe");
        write(&root, "odbc", &manifest("odbc", ""));
        let folder = root.join("odbc");
        let path = folder.join(MANIFEST);
        let unix = load(&folder, &path, Layout::Plain, false).unwrap();
        let windows = load(&folder, &path, Layout::Plain, true).unwrap();
        assert!(unix.executable.ends_with("bin/run"));
        assert!(windows.executable.ends_with("bin/run.exe"));
    }

    #[test]
    fn a_bundled_manifest_resolves_into_helpers() {
        let root = scratch("bundle")
            .join("Contents")
            .join("Resources")
            .join("plugins");
        write(&root, "odbc", &manifest("odbc", ""));
        let found = discover_in(&[(root.clone(), Layout::MacBundleResources)]);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].executable,
            root.join("odbc/../../../Helpers")
                .join(format!("run{}", std::env::consts::EXE_SUFFIX))
        );
    }
}
