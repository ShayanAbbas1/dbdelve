//! Where a build keeps its connections' passwords and key passphrases.
//!
//! On macOS they all live in one generic-password item per variant, the
//! vault. A build is signed ad hoc, so "Always Allow" on an item trusts that
//! build alone, and an update asks again once for each item it reads: with
//! one item, once. Linux and Windows keep an item per secret, as every build
//! before this did on every platform: neither asks again after an update, and
//! Credential Manager caps an item at 2560 bytes, which a vault would outgrow.
//!
//! A secret saved in its own item by an earlier build is moved into the vault
//! the first time it is read, and its item deleted once the vault holds it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::{data_root, passphrase_service};

/// Whether this platform keeps a build's secrets in one item.
pub(crate) const ONE_ITEM: bool = cfg!(target_os = "macos");

/// The keychain one item at a time, by service and account.
pub(crate) trait Keychain {
    /// `None` for an item that isn't there.
    fn read(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String>;
    /// `Ok` for an item that isn't there.
    fn delete(&self, service: &str, account: &str) -> Result<(), String>;
}

/// Keychain Services, Secret Service or Credential Manager. On macOS a read
/// can raise a prompt, for an item another build saved: run it off the UI
/// thread.
pub(crate) struct SystemKeychain;

impl Keychain for SystemKeychain {
    fn read(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(service, account).and_then(|entry| entry.get_password()) {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(described(&error)),
        }
    }

    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(service, account)
            .and_then(|entry| entry.set_password(secret))
            .map_err(|error| described(&error))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        match keyring::Entry::new(service, account).and_then(|entry| entry.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(described(&error)),
        }
    }
}

/// What went wrong, and nothing else: a keyring error's own text can carry
/// the account, which is named after the connection, and the `Ambiguous` one
/// the matching items themselves.
pub(crate) fn kind_of(error: &keyring::Error) -> &'static str {
    match error {
        keyring::Error::PlatformFailure(_) => "the keychain failed",
        keyring::Error::NoStorageAccess(_) => "the keychain could not be opened",
        keyring::Error::NoEntry => "no saved item",
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => {
            "the saved item could not be read"
        }
        keyring::Error::BadStoreFormat(_) => "the keychain could not be read",
        keyring::Error::TooLong(..) => "a name is too long for the keychain",
        keyring::Error::Invalid(..) => "the keychain refused the item",
        keyring::Error::Ambiguous(_) => "more than one saved item matched",
        _ => "an unknown keychain error",
    }
}

/// [`kind_of`], and for a failure of the platform's own its message too, which
/// is what tells a denied prompt from a locked keychain. That message comes
/// from the platform's status code, not from the item.
fn described(error: &keyring::Error) -> String {
    match error {
        keyring::Error::PlatformFailure(inner) | keyring::Error::NoStorageAccess(inner) => {
            format!("{}: {inner}", kind_of(error))
        }
        _ => kind_of(error).to_string(),
    }
}

/// What a profile keeps in the keychain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Secret {
    Password,
    SshPassphrase,
}

impl Secret {
    pub(crate) const BOTH: [Self; 2] = [Self::Password, Self::SshPassphrase];

    pub(crate) fn what(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::SshPassphrase => "SSH key passphrase",
        }
    }

    /// The service of an item that holds one secret of this kind, under the
    /// profile id as its account: where every secret lives off macOS, and
    /// where a build on macOS kept it before the vault.
    pub(crate) fn item_service(self, variant: &str) -> String {
        match self {
            Self::Password => variant.to_string(),
            Self::SshPassphrase => passphrase_service(variant),
        }
    }
}

/// A `/` for the reason [`passphrase_service`] has one, and a name apart from
/// it, so no profile id names the vault as an item of its own.
pub(crate) fn vault_service(variant: &str) -> String {
    format!("{variant}/secrets")
}

const VAULT_ACCOUNT: &str = "vault";

const VAULT_VERSION: u32 = 1;

/// Every secret a build keeps, by profile id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Vault {
    version: u32,
    #[serde(default)]
    profiles: BTreeMap<String, Saved>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssh_passphrase: Option<String>,
}

impl Saved {
    fn slot(&mut self, kind: Secret) -> &mut Option<String> {
        match kind {
            Secret::Password => &mut self.password,
            Secret::SshPassphrase => &mut self.ssh_passphrase,
        }
    }
}

impl Vault {
    pub(crate) fn get(&self, profile_id: &str, kind: Secret) -> Option<&str> {
        let saved = self.profiles.get(profile_id)?;
        match kind {
            Secret::Password => saved.password.as_deref(),
            Secret::SshPassphrase => saved.ssh_passphrase.as_deref(),
        }
    }

    fn set(&mut self, profile_id: &str, kind: Secret, secret: &str) {
        *self
            .profiles
            .entry(profile_id.to_string())
            .or_default()
            .slot(kind) = Some(secret.to_string());
    }

    fn remove(&mut self, profile_id: &str) {
        self.profiles.remove(profile_id);
    }

    fn decode(text: &str) -> Result<Self, String> {
        let vault: Self =
            serde_json::from_str(text).map_err(|_| "the saved item could not be read")?;
        // Written back, a newer vault would lose whatever this build can't
        // read in it.
        if vault.version > VAULT_VERSION {
            return Err("the saved item is from a newer DBDelve".into());
        }
        Ok(vault)
    }

    fn encode(&self) -> String {
        let mut vault = self.clone();
        vault.version = VAULT_VERSION;
        serde_json::to_string(&vault).expect("a map of strings serializes")
    }

    /// The vault `variant` keeps now, read past any cache. Empty for a build
    /// that never saved one.
    pub(crate) fn load(keychain: &impl Keychain, variant: &str) -> Result<Self, String> {
        match keychain.read(&vault_service(variant), VAULT_ACCOUNT)? {
            Some(text) => Self::decode(&text),
            None => Ok(Self::default()),
        }
    }
}

/// Each variant's vault, read once for the life of the process, as that is
/// the read that can prompt. One that failed to read is not kept, so a later
/// read tries again, as a denied prompt or a locked keychain may since have
/// changed. A secret this copy lacks is looked for in the item again, as
/// another running instance may have saved it since.
///
/// A change reads the item again, writes it back whole and deletes the items
/// it replaced, all under a lock file every instance of the variant takes, so
/// no instance writes over a change another made meanwhile.
pub(crate) struct Vaults {
    loaded: Mutex<BTreeMap<String, Vault>>,
    locks: Locks,
}

/// Where the lock files are.
enum Locks {
    /// Beside each variant's data directory, as the instance lock is: an
    /// import moves the directory.
    DataRoot,
    #[cfg(test)]
    In(std::path::PathBuf),
}

pub(crate) static VAULTS: Vaults = Vaults::new();

impl Vaults {
    const fn new() -> Self {
        Self {
            loaded: Mutex::new(BTreeMap::new()),
            locks: Locks::DataRoot,
        }
    }

    /// Locking in a directory under the system's temporary one, shared by
    /// every test in the process, so a test never reaches the data root.
    #[cfg(test)]
    pub(crate) fn scratch() -> Self {
        Self {
            loaded: Mutex::new(BTreeMap::new()),
            locks: Locks::In(
                std::env::temp_dir().join(format!("dbdelve-vault-locks-{}", std::process::id())),
            ),
        }
    }

    fn cache(&self) -> MutexGuard<'_, BTreeMap<String, Vault>> {
        self.loaded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Held until dropped, as the lock goes with the file.
    fn hold(&self, variant: &str) -> Result<File, String> {
        let directory = match &self.locks {
            Locks::DataRoot => data_root()?,
            #[cfg(test)]
            Locks::In(directory) => directory.clone(),
        };
        let failed = |error: io::Error| format!("the saved passwords could not be locked: {error}");
        fs::create_dir_all(&directory).map_err(failed)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(format!("{variant}.secrets.lock")))
            .map_err(failed)?;
        file.lock().map_err(failed)?;
        Ok(file)
    }

    /// `change` made to the vault as the keychain holds it now, written back
    /// whole, and kept as this process's copy. Returns what `change` did, and
    /// the lock, for whatever else belongs to the change to finish under it.
    fn update<T>(
        &self,
        loaded: &mut BTreeMap<String, Vault>,
        keychain: &impl Keychain,
        variant: &str,
        change: impl FnOnce(&mut Vault) -> T,
    ) -> Result<(T, File), String> {
        let held = self.hold(variant)?;
        let current = Vault::load(keychain, variant)?;
        let mut vault = current.clone();
        let changed = change(&mut vault);
        if vault != current {
            keychain.write(&vault_service(variant), VAULT_ACCOUNT, &vault.encode())?;
        }
        loaded.insert(variant.to_string(), vault);
        Ok((changed, held))
    }

    /// `Ok(None)` only when neither the vault nor an item of the profile's own
    /// holds one. A secret found in its own item is moved into the vault, and
    /// the item deleted only once the vault holds it.
    pub(crate) fn read(
        &self,
        keychain: &impl Keychain,
        variant: &str,
        kind: Secret,
        profile_id: &str,
    ) -> Result<Option<String>, String> {
        // Held across the keychain calls, so reads waiting on the first one's
        // prompt don't each raise their own.
        let mut loaded = self.cache();
        if let Some(secret) = loaded
            .get(variant)
            .and_then(|vault| vault.get(profile_id, kind))
        {
            return Ok(Some(secret.to_string()));
        }
        let vault = Vault::load(keychain, variant)?;
        let found = vault.get(profile_id, kind).map(str::to_string);
        loaded.insert(variant.to_string(), vault);
        if found.is_some() {
            return Ok(found);
        }
        let service = kind.item_service(variant);
        let Some(legacy) = keychain.read(&service, profile_id)? else {
            return Ok(None);
        };
        // Another instance may have saved one between the load above and this
        // lock, which is newer than the item.
        let moved = self.update(&mut loaded, keychain, variant, |vault| {
            match vault.get(profile_id, kind) {
                Some(saved) => saved.to_string(),
                None => {
                    vault.set(profile_id, kind, &legacy);
                    legacy.clone()
                }
            }
        });
        match moved {
            Ok((secret, _held)) => {
                delete_item(keychain, &service, profile_id);
                Ok(Some(secret))
            }
            Err(error) => {
                tracing::warn!("a saved {} stays in its own item: {error}", kind.what());
                Ok(Some(legacy))
            }
        }
    }

    pub(crate) fn write(
        &self,
        keychain: &impl Keychain,
        variant: &str,
        kind: Secret,
        profile_id: &str,
        secret: &str,
    ) -> Result<(), String> {
        self.write_all(keychain, variant, &[(profile_id, kind, secret)])
    }

    /// Every one of `secrets` in one write of the vault, after which none of
    /// them has an item of its own left over to come back from.
    pub(crate) fn write_all(
        &self,
        keychain: &impl Keychain,
        variant: &str,
        secrets: &[(&str, Secret, &str)],
    ) -> Result<(), String> {
        if secrets.is_empty() {
            return Ok(());
        }
        let mut loaded = self.cache();
        let ((), _held) = self.update(&mut loaded, keychain, variant, |vault| {
            for (profile_id, kind, secret) in secrets {
                vault.set(profile_id, *kind, secret);
            }
        })?;
        for (profile_id, kind, _) in secrets {
            delete_item(keychain, &kind.item_service(variant), profile_id);
        }
        Ok(())
    }

    /// Every secret a profile keeps, in the vault and in items of its own.
    pub(crate) fn forget(&self, keychain: &impl Keychain, variant: &str, profile_id: &str) {
        let mut loaded = self.cache();
        let _held = match self.update(&mut loaded, keychain, variant, |vault| {
            vault.remove(profile_id)
        }) {
            Ok(((), held)) => Some(held),
            Err(error) => {
                tracing::warn!("a deleted connection's secrets stay in the keychain: {error}");
                None
            }
        };
        for kind in Secret::BOTH {
            delete_item(keychain, &kind.item_service(variant), profile_id);
        }
    }
}

/// Best effort: an item left behind is only read when the vault has nothing
/// for it.
fn delete_item(keychain: &impl Keychain, service: &str, profile_id: &str) {
    if let Err(error) = keychain.delete(service, profile_id) {
        tracing::warn!("a saved item the vault replaced was not deleted: {error}");
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use super::Keychain;

    type Key = (String, String);

    fn key(service: &str, account: &str) -> Key {
        (service.to_string(), account.to_string())
    }

    fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// A keychain in memory, with items that refuse to be read or written,
    /// as a denied prompt does, and that can be slow to read or change while
    /// another is read, as another process's write would.
    #[derive(Default)]
    pub(crate) struct FakeKeychain {
        items: Mutex<HashMap<Key, String>>,
        unreadable: Mutex<Vec<Key>>,
        unwritable: Mutex<Vec<Key>>,
        slow: Mutex<Vec<(Key, Duration)>>,
        meanwhile: Mutex<Vec<(Key, Key, String)>>,
    }

    impl FakeKeychain {
        pub(crate) fn with(items: &[(&str, &str, &str)]) -> Self {
            let keychain = Self::default();
            for (service, account, secret) in items {
                keychain.put(service, account, secret);
            }
            keychain
        }

        pub(crate) fn get(&self, service: &str, account: &str) -> Option<String> {
            locked(&self.items).get(&key(service, account)).cloned()
        }

        pub(crate) fn len(&self) -> usize {
            locked(&self.items).len()
        }

        pub(crate) fn put(&self, service: &str, account: &str, secret: &str) {
            locked(&self.items).insert(key(service, account), secret.to_string());
        }

        pub(crate) fn refuse_reads(&self, service: &str, account: &str, refused: bool) {
            Self::refuse(&self.unreadable, service, account, refused);
        }

        pub(crate) fn refuse_writes(&self, service: &str, account: &str, refused: bool) {
            Self::refuse(&self.unwritable, service, account, refused);
        }

        fn refuse(list: &Mutex<Vec<Key>>, service: &str, account: &str, refused: bool) {
            let key = key(service, account);
            let mut list = locked(list);
            list.retain(|listed| listed != &key);
            if refused {
                list.push(key);
            }
        }

        /// Every read of the item takes `delay`.
        pub(crate) fn slow_reads(&self, service: &str, account: &str, delay: Duration) {
            locked(&self.slow).push((key(service, account), delay));
        }

        /// The next read of `read` first puts `secret` in the other item.
        pub(crate) fn meanwhile(&self, read: (&str, &str), put: (&str, &str), secret: &str) {
            locked(&self.meanwhile).push((
                key(read.0, read.1),
                key(put.0, put.1),
                secret.to_string(),
            ));
        }
    }

    impl Keychain for FakeKeychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            let read = key(service, account);
            if locked(&self.unreadable).contains(&read) {
                return Err("User canceled the operation.".into());
            }
            let delay = locked(&self.slow)
                .iter()
                .find(|(slow, _)| slow == &read)
                .map(|(_, delay)| *delay);
            let value = self.get(service, account);
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            let mut meanwhile = locked(&self.meanwhile);
            if let Some(at) = meanwhile.iter().position(|(on, ..)| on == &read) {
                let (_, (service, account), secret) = meanwhile.remove(at);
                drop(meanwhile);
                self.put(&service, &account, &secret);
            }
            Ok(value)
        }

        fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
            if locked(&self.unwritable).contains(&key(service, account)) {
                return Err("User canceled the operation.".into());
            }
            self.put(service, account, secret);
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), String> {
            locked(&self.items).remove(&key(service, account));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeKeychain;
    use super::*;

    const VARIANT: &str = "dbdelve";

    fn vault_of(keychain: &FakeKeychain) -> Vault {
        Vault::load(keychain, VARIANT).unwrap()
    }

    #[test]
    fn the_vaults_service_is_no_other_services() {
        let vault = vault_service(VARIANT);
        assert_ne!(vault, VARIANT);
        assert_ne!(vault, passphrase_service(VARIANT));
        assert!(vault.contains('/'));
    }

    #[test]
    fn secrets_written_read_back_from_one_item() {
        let keychain = FakeKeychain::default();
        let vaults = Vaults::scratch();
        vaults
            .write(&keychain, VARIANT, Secret::Password, "a", "pw-a")
            .unwrap();
        vaults
            .write(&keychain, VARIANT, Secret::SshPassphrase, "a", "pass-a")
            .unwrap();
        vaults
            .write(&keychain, VARIANT, Secret::Password, "b", "")
            .unwrap();

        let fresh = Vaults::scratch();
        let read = |kind, id| fresh.read(&keychain, VARIANT, kind, id).unwrap();
        assert_eq!(read(Secret::Password, "a").as_deref(), Some("pw-a"));
        assert_eq!(read(Secret::SshPassphrase, "a").as_deref(), Some("pass-a"));
        assert_eq!(
            read(Secret::Password, "b").as_deref(),
            Some(""),
            "a blank password is one"
        );
        assert_eq!(read(Secret::SshPassphrase, "b"), None);
        assert_eq!(read(Secret::Password, "c"), None);

        assert_eq!(keychain.len(), 1);
        assert_eq!(keychain.get("dbdelve", "a"), None);
    }

    #[test]
    fn a_secret_in_its_own_item_moves_into_the_vault_and_its_item_goes() {
        let keychain = FakeKeychain::with(&[
            ("dbdelve", "a", "pw-a"),
            ("dbdelve/ssh-passphrase", "a", "pass-a"),
        ]);
        let vaults = Vaults::scratch();

        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("pw-a")
        );
        assert_eq!(vault_of(&keychain).get("a", Secret::Password), Some("pw-a"));
        assert_eq!(keychain.get("dbdelve", "a"), None);
        assert_eq!(
            keychain.get("dbdelve/ssh-passphrase", "a").as_deref(),
            Some("pass-a"),
            "only what was read moves"
        );

        keychain.refuse_reads("dbdelve", "a", true);
        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("pw-a"),
            "the vault answers now"
        );
    }

    #[test]
    fn a_vault_that_wont_take_the_secret_leaves_its_item_in_place() {
        let keychain = FakeKeychain::with(&[("dbdelve", "a", "pw-a")]);
        keychain.refuse_writes("dbdelve/secrets", "vault", true);
        let vaults = Vaults::scratch();

        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("pw-a")
        );
        assert_eq!(keychain.get("dbdelve", "a").as_deref(), Some("pw-a"));
        assert_eq!(keychain.get("dbdelve/secrets", "vault"), None);

        keychain.refuse_writes("dbdelve/secrets", "vault", false);
        vaults
            .read(&keychain, VARIANT, Secret::Password, "a")
            .unwrap();
        assert_eq!(keychain.get("dbdelve", "a"), None, "moved at the next read");
        assert_eq!(vault_of(&keychain).get("a", Secret::Password), Some("pw-a"));
    }

    #[test]
    fn a_vault_that_wont_read_is_an_error_and_is_read_again_next_time() {
        let keychain = FakeKeychain::default();
        Vaults::scratch()
            .write(&keychain, VARIANT, Secret::Password, "a", "pw-a")
            .unwrap();
        keychain.refuse_reads("dbdelve/secrets", "vault", true);
        let vaults = Vaults::scratch();

        assert!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .is_err()
        );
        assert!(
            vaults
                .write(&keychain, VARIANT, Secret::Password, "b", "pw-b")
                .is_err(),
            "nothing is written over a vault that didn't read"
        );

        keychain.refuse_reads("dbdelve/secrets", "vault", false);
        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("pw-a")
        );
    }

    #[test]
    fn a_vault_that_doesnt_parse_or_is_newer_is_never_written_over() {
        for text in ["not json", r#"{"version":2,"profiles":{}}"#] {
            let keychain = FakeKeychain::with(&[("dbdelve/secrets", "vault", text)]);
            let vaults = Vaults::scratch();
            assert!(
                vaults
                    .read(&keychain, VARIANT, Secret::Password, "a")
                    .is_err()
            );
            assert!(
                vaults
                    .write(&keychain, VARIANT, Secret::Password, "a", "pw")
                    .is_err()
            );
            assert_eq!(
                keychain.get("dbdelve/secrets", "vault").as_deref(),
                Some(text)
            );
        }
    }

    #[test]
    fn a_write_keeps_what_another_process_wrote_since_this_one_read() {
        let keychain = FakeKeychain::default();
        let here = Vaults::scratch();
        let there = Vaults::scratch();
        here.write(&keychain, VARIANT, Secret::Password, "a", "pw-a")
            .unwrap();
        there
            .write(&keychain, VARIANT, Secret::Password, "b", "pw-b")
            .unwrap();
        here.write(&keychain, VARIANT, Secret::Password, "c", "pw-c")
            .unwrap();

        let vault = vault_of(&keychain);
        for (id, password) in [("a", "pw-a"), ("b", "pw-b"), ("c", "pw-c")] {
            assert_eq!(vault.get(id, Secret::Password), Some(password));
        }
    }

    #[test]
    fn instances_changing_the_vault_at_once_take_turns() {
        let variant = "dbdelve-turns";
        let keychain = FakeKeychain::default();
        keychain.slow_reads(
            &vault_service(variant),
            VAULT_ACCOUNT,
            std::time::Duration::from_millis(100),
        );
        std::thread::scope(|scope| {
            for id in ["a", "b"] {
                let keychain = &keychain;
                scope.spawn(move || {
                    Vaults::scratch()
                        .write(keychain, variant, Secret::Password, id, id)
                        .unwrap();
                });
            }
        });

        let vault = Vault::load(&keychain, variant).unwrap();
        assert_eq!(vault.get("a", Secret::Password), Some("a"));
        assert_eq!(vault.get("b", Secret::Password), Some("b"));
    }

    #[test]
    fn a_secret_another_instance_saved_is_read_though_this_one_had_none() {
        let keychain = FakeKeychain::default();
        let here = Vaults::scratch();
        let there = Vaults::scratch();
        assert_eq!(
            here.read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap(),
            None
        );
        there
            .write(&keychain, VARIANT, Secret::Password, "a", "pw-a")
            .unwrap();
        assert_eq!(
            here.read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("pw-a")
        );
    }

    #[test]
    fn an_item_moving_in_never_replaces_what_another_instance_saved_meanwhile() {
        let keychain = FakeKeychain::with(&[("dbdelve", "a", "old")]);
        keychain.meanwhile(
            ("dbdelve", "a"),
            ("dbdelve/secrets", "vault"),
            r#"{"version":1,"profiles":{"a":{"password":"new"}}}"#,
        );
        let vaults = Vaults::scratch();

        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("new")
        );
        assert_eq!(vault_of(&keychain).get("a", Secret::Password), Some("new"));
        assert_eq!(keychain.get("dbdelve", "a"), None, "the item still goes");

        keychain.refuse_reads("dbdelve/secrets", "vault", true);
        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("new"),
            "and this process's copy has it"
        );
    }

    #[test]
    fn a_write_deletes_the_item_it_replaces() {
        let keychain = FakeKeychain::with(&[("dbdelve", "a", "old")]);
        let vaults = Vaults::scratch();
        vaults
            .write(&keychain, VARIANT, Secret::Password, "a", "new")
            .unwrap();
        assert_eq!(keychain.get("dbdelve", "a"), None);
        assert_eq!(
            Vaults::scratch()
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn forgetting_a_profile_clears_the_vault_and_its_own_items() {
        let keychain = FakeKeychain::with(&[
            ("dbdelve", "a", "old"),
            ("dbdelve/ssh-passphrase", "a", "pass-a"),
            ("dbdelve", "b", "pw-b"),
        ]);
        let vaults = Vaults::scratch();
        vaults
            .write(&keychain, VARIANT, Secret::Password, "a", "pw-a")
            .unwrap();
        vaults
            .write(&keychain, VARIANT, Secret::Password, "c", "pw-c")
            .unwrap();

        vaults.forget(&keychain, VARIANT, "a");

        for kind in Secret::BOTH {
            assert_eq!(vaults.read(&keychain, VARIANT, kind, "a").unwrap(), None);
            assert_eq!(keychain.get(&kind.item_service(VARIANT), "a"), None);
        }
        assert_eq!(vault_of(&keychain).get("c", Secret::Password), Some("pw-c"));
        assert_eq!(keychain.get("dbdelve", "b").as_deref(), Some("pw-b"));
    }

    #[test]
    fn each_variant_has_a_vault_of_its_own() {
        let keychain = FakeKeychain::default();
        let vaults = Vaults::scratch();
        vaults
            .write(&keychain, "dbdelve-dev", Secret::Password, "a", "dev")
            .unwrap();
        assert_eq!(
            vaults
                .read(&keychain, VARIANT, Secret::Password, "a")
                .unwrap(),
            None
        );
        assert_eq!(
            vaults
                .read(&keychain, "dbdelve-dev", Secret::Password, "a")
                .unwrap()
                .as_deref(),
            Some("dev")
        );
    }
}
