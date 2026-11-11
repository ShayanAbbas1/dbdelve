//! ssh's askpass, for a key with a passphrase. ssh runs DBDelve's own
//! executable with its prompt, and that process fetches the passphrase from
//! the DBDelve that started ssh, over loopback and by a one-time token, so the
//! passphrase is never in ssh's environment or arguments, where any process
//! of the same user could read it.
//!
//! TCP rather than a Unix socket, so the same path serves Windows.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Where the askpass process asks, and the token it shows.
const ADDRESS: &str = "DBDELVE_ASKPASS";
const TOKEN: &str = "DBDELVE_ASKPASS_TOKEN";

/// What ssh asks its askpass for a key file's passphrase, and the one prompt
/// answered: a server's password or keyboard-interactive prompt, a host key
/// question and a hardware key's PIN never get the passphrase. OpenSSH 8.4,
/// the first to honour `SSH_ASKPASS_REQUIRE`, also began every
/// keyboard-interactive prompt with `(user@host)`, so a server cannot spell
/// this one.
const KEY_PROMPT: &str = "Enter passphrase for key ";

/// How long either side waits on the other, so a stalled askpass cannot hold
/// up the next.
const ANSWERED_WITHIN: Duration = Duration::from_secs(5);

/// The passphrase, handed to whoever shows the token on a loopback port, for
/// as long as this lives.
pub(super) struct Askpass {
    address: SocketAddr,
    token: String,
    program: OsString,
    answered: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    serving: Option<JoinHandle<()>>,
}

impl Askpass {
    pub(super) fn serve(passphrase: &str) -> io::Result<Self> {
        let program = std::env::current_exe()?.into_os_string();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let token = token()?;
        let answered = Arc::<AtomicBool>::default();
        let stop = Arc::<AtomicBool>::default();
        let serving = thread::spawn({
            let token = token.clone();
            let passphrase = passphrase.to_owned();
            let answered = answered.clone();
            let stop = stop.clone();
            move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if answer(&stream, &token, &passphrase).unwrap_or(false) {
                                answered.store(true, Ordering::Relaxed);
                            }
                        }
                        Err(_) => thread::sleep(Duration::from_millis(20)),
                    }
                }
            }
        });
        Ok(Self {
            address,
            token,
            program,
            answered,
            stop,
            serving: Some(serving),
        })
    }

    /// Has ssh run this executable as its askpass, every prompt included, and
    /// told it where to ask.
    ///
    /// ponytail: an ssh before OpenSSH 8.4 ignores `SSH_ASKPASS_REQUIRE`, and
    /// Windows' own OpenSSH may too; such an ssh asks its askpass only with
    /// `DISPLAY` set and no terminal, and otherwise fails the login as it did
    /// before a passphrase could be given. Telling the user which ssh is too
    /// old, from `ssh -V`, is the upgrade if that is ever reported.
    pub(super) fn put_on(&self, command: &mut Command) {
        command
            .env("SSH_ASKPASS", &self.program)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(ADDRESS, self.address.to_string())
            .env(TOKEN, &self.token);
    }

    /// Whether ssh was handed the passphrase at least once.
    pub(super) fn answered(&self) -> bool {
        self.answered.load(Ordering::Relaxed)
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
    }
}

/// Whether `stream` showed the token and was given the passphrase. A wrong
/// token is closed on without a word.
fn answer(mut stream: &TcpStream, token: &str, passphrase: &str) -> io::Result<bool> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(ANSWERED_WITHIN))?;
    stream.set_write_timeout(Some(ANSWERED_WITHIN))?;
    let mut shown = Vec::new();
    // One byte past a token, so a longer one cannot pass for it.
    stream
        .take(token.len() as u64 + 1)
        .read_to_end(&mut shown)?;
    if !same(&shown, token.as_bytes()) {
        return Ok(false);
    }
    stream.write_all(passphrase.as_bytes())?;
    Ok(true)
}

/// Every byte compared whatever the first difference, so how long a wrong
/// token took to refuse says nothing about the right one.
fn same(shown: &[u8], token: &[u8]) -> bool {
    shown.len() == token.len()
        && shown
            .iter()
            .zip(token)
            .fold(0, |differs, (shown, token)| differs | (shown ^ token))
            == 0
}

/// 128 bits from the OS, as hex.
fn token() -> io::Result<String> {
    use ring::rand::{SecureRandom as _, SystemRandom};

    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("the OS gave no random bytes"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The askpass process's whole run, when ssh started this executable as one:
/// its exit status, having printed the passphrase or nothing. None when this
/// is not an askpass process.
pub fn askpass(prompt: Option<&OsStr>) -> Option<i32> {
    let address = std::env::var_os(ADDRESS)?;
    let token = std::env::var_os(TOKEN).unwrap_or_default();
    let asked = ask(
        &address.to_string_lossy(),
        &token.to_string_lossy(),
        &prompt.unwrap_or_default().to_string_lossy(),
    );
    let Some(passphrase) = asked else {
        return Some(1);
    };
    let mut stdout = io::stdout().lock();
    Some(
        match writeln!(stdout, "{passphrase}").and_then(|()| stdout.flush()) {
            Ok(()) => 0,
            Err(_) => 1,
        },
    )
}

/// The passphrase the DBDelve at `address` gives for `token`, for a key
/// file's prompt only.
fn ask(address: &str, token: &str, prompt: &str) -> Option<String> {
    if !prompt.starts_with(KEY_PROMPT) {
        return None;
    }
    let mut stream = TcpStream::connect_timeout(&address.parse().ok()?, ANSWERED_WITHIN).ok()?;
    stream.set_read_timeout(Some(ANSWERED_WITHIN)).ok()?;
    stream.write_all(token.as_bytes()).ok()?;
    stream.shutdown(Shutdown::Write).ok()?;
    let mut passphrase = String::new();
    stream.read_to_string(&mut passphrase).ok()?;
    (!passphrase.is_empty()).then_some(passphrase)
}

/// ssh runs the test binary as its askpass in the live tests, and libtest
/// would read the prompt as a test filter: this answers first, before
/// libtest's own main, as `main` does in the app. The prompt comes from the
/// loader's own arguments, since std may not have read them yet.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
static ASKPASS_BEFORE_LIBTEST: unsafe extern "C" fn(
    std::ffi::c_int,
    *const *const std::ffi::c_char,
) = {
    unsafe extern "C" fn before_libtest(
        argc: std::ffi::c_int,
        argv: *const *const std::ffi::c_char,
    ) {
        if std::env::var_os(ADDRESS).is_none() {
            return;
        }
        // SAFETY: the loader passes the process's own argc and argv, each a
        // NUL-terminated string.
        let prompt = (argc > 1 && !argv.is_null()).then(|| unsafe {
            use std::os::unix::ffi::OsStrExt as _;
            OsStr::from_bytes(std::ffi::CStr::from_ptr(*argv.add(1)).to_bytes()).to_owned()
        });
        if let Some(status) = askpass(prompt.as_deref()) {
            std::process::exit(status);
        }
    }
    before_libtest
};

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "Enter passphrase for key '/home/dev/.ssh/id_ed25519': ";

    #[test]
    fn the_right_token_is_given_the_passphrase_and_a_wrong_one_nothing() {
        let askpass = Askpass::serve("correct horse").unwrap();
        let address = askpass.address.to_string();

        assert_eq!(ask(&address, &"0".repeat(32), PROMPT), None);
        assert_eq!(ask(&address, &askpass.token[..31], PROMPT), None);
        assert_eq!(ask(&address, &format!("{}0", askpass.token), PROMPT), None);
        assert!(!askpass.answered());

        assert_eq!(
            ask(&address, &askpass.token, PROMPT).as_deref(),
            Some("correct horse")
        );
        assert!(askpass.answered());
        // ssh asks again after a passphrase that did not decrypt the key.
        assert_eq!(
            ask(&address, &askpass.token, PROMPT).as_deref(),
            Some("correct horse")
        );

        drop(askpass);
        assert_eq!(ask(&address, "", PROMPT), None);
    }

    #[test]
    fn only_a_key_files_prompt_is_answered() {
        let askpass = Askpass::serve("correct horse").unwrap();
        let address = askpass.address.to_string();
        for prompt in [
            "dev@bastion's password: ",
            "(dev@bastion) Enter passphrase for key '/k': ",
            "(dev@bastion) Password: ",
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "Enter PIN for ED25519-SK key /home/dev/.ssh/id_ed25519_sk: ",
            "Confirm user presence for key ED25519-SK SHA256:x",
            "",
        ] {
            assert_eq!(ask(&address, &askpass.token, prompt), None, "{prompt}");
        }
        assert!(!askpass.answered());
    }

    #[test]
    fn a_token_is_128_random_bits() {
        let token = token().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(token, super::token().unwrap());
    }
}
