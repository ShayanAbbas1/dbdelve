//! A small re-exec watchdog: its stdin belongs only to the app. EOF ends the
//! entire CLI process group, including the plugin, even after an app crash.
use std::{
    io::{self, Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
const COMMAND: &str = "DBDELVE_SSM_COMMAND";

pub(super) struct Process {
    pub child: Option<Child>,
}
impl Process {
    pub fn spawn(command: Command) -> io::Result<Self> {
        let program = command
            .get_program()
            .to_str()
            .ok_or_else(|| io::Error::other("Executable path must be UTF-8."))?;
        let args: Vec<_> = command
            .get_args()
            .map(|arg| {
                arg.to_str()
                    .ok_or_else(|| io::Error::other("Argument must be UTF-8."))
            })
            .collect::<Result<_, _>>()?;
        let payload = serde_json::to_string(&(program, args))?;
        let mut helper = Command::new(std::env::current_exe()?);
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => {
                    helper.env(key, value);
                }
                None => {
                    helper.env_remove(key);
                }
            }
        }
        helper
            .env(COMMAND, payload)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // A terminal can terminate the owner's entire group. The watchdog
            // must survive it to see EOF and end the CLI's separate group.
            helper.process_group(0);
            super::super::ssh::unmask_signals(&mut helper);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            helper.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        if let Some(directory) = command.get_current_dir() {
            helper.current_dir(directory);
        }
        Ok(Self {
            child: Some(crate::db::spawn(&mut helper)?),
        })
    }
    pub fn stop(&mut self) {
        if let Some(child) = &mut self.child {
            drop(child.stdin.take());
        }
    }
    pub fn running(&mut self) -> bool {
        matches!(
            self.child.as_mut().expect("live child").try_wait(),
            Ok(None)
        )
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Called before GPUI starts, and before libtest in process-lifecycle tests.
pub fn helper() -> Option<i32> {
    let payload = std::env::var(COMMAND).ok()?;
    // Also needed before Rust's runtime in the libtest constructor. Reporting
    // an exit into a closed output pipe must never bypass descendant cleanup.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let result = run(&payload);
    Some(match result {
        Ok(code) => code,
        Err(_) => {
            let _ = writeln!(io::stderr(), "Could not start SSM child process.");
            1
        }
    })
}
fn run(payload: &str) -> io::Result<i32> {
    let (program, args): (String, Vec<String>) = serde_json::from_str(payload)?;
    let mut command = Command::new(program);
    command.args(args).env_remove(COMMAND).stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        super::super::ssh::unmask_signals(&mut command);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = crate::db::spawn(&mut command)?;
    #[cfg(windows)]
    let job = match super::super::ssh::kill_on_close_job(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let ended = Arc::new(AtomicBool::new(false));
    let eof = ended.clone();
    thread::spawn(move || {
        let _ = io::stdin().read(&mut [0u8]);
        eof.store(true, Ordering::Release);
    });
    let status = loop {
        if ended.load(Ordering::Acquire) {
            break None;
        }
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let _ = writeln!(io::stdout(), "DBDELVE_SSM_CHILD_EXITED");
    let _ = io::stdout().flush();
    #[cfg(unix)]
    {
        // The group belongs to this child; argv is never interpreted by a shell.
        // TERM lets the plugin send its remote TerminateSession flag; KILL is
        // the bounded fallback for credential processes or a stuck CLI.
        let group = -(child.id() as i32);
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        thread::sleep(Duration::from_millis(500));
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    drop(job);
    let _ = child.wait();
    Ok(status.and_then(|s| s.code()).unwrap_or(1))
}

#[cfg(all(
    test,
    any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")
))]
#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
static BEFORE_LIBTEST: extern "C" fn() = {
    extern "C" fn before() {
        if std::env::var_os("DBDELVE_TEST_SSM_OWNER").is_some() {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "echo $$; exec sleep 60"])
                .env_remove("DBDELVE_TEST_SSM_OWNER");
            let mut process = Process::spawn(command).unwrap();
            use std::io::BufRead;
            let mut pid = String::new();
            io::BufReader::new(process.child.as_mut().unwrap().stdout.take().unwrap())
                .read_line(&mut pid)
                .unwrap();
            let _ = write!(io::stdout(), "{pid}");
            let _ = io::stdout().flush();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        if let Some(code) = super::fake_cli() {
            std::process::exit(code);
        }
        if let Some(code) = helper() {
            std::process::exit(code);
        }
    }
    before
};

#[cfg(all(test, unix, any(target_os = "macos", target_env = "gnu")))]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    fn owner_process_group_shutdown_still_cleans_up_cli() {
        use std::{io::BufRead, os::unix::process::CommandExt};
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env("DBDELVE_TEST_SSM_OWNER", "1")
            .process_group(0)
            .stdout(Stdio::piped());
        super::super::super::ssh::unmask_signals(&mut command);
        let mut owner = crate::db::spawn(&mut command).unwrap();
        let mut line = String::new();
        io::BufReader::new(owner.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let cli = line.trim().parse::<i32>().unwrap();
        unsafe {
            libc::kill(-(owner.id() as i32), libc::SIGTERM);
        }
        owner.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(cli, 0) } == 0 {
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(-cli, libc::SIGKILL);
                }
                panic!("CLI survived owner-group shutdown");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn watchdog_ends_grandchild_on_owner_drop() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & echo $!; wait"]);
        let mut process = Process::spawn(command).unwrap();
        use std::io::BufRead;
        let mut line = String::new();
        io::BufReader::new(process.child.as_mut().unwrap().stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid = line.trim().parse::<i32>().unwrap();
        assert!(process.running());
        process.stop();
        let status = process.child.as_mut().unwrap().wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_ne!(
            status.signal(),
            Some(libc::SIGPIPE),
            "watchdog must clean up even when output closes"
        );
        drop(process);
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "grandchild survived teardown: {:?}",
                Command::new("ps")
                    .args(["-o", "pid=,ppid=,stat=,comm=", "-p", &pid.to_string()])
                    .output()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn watchdog_reports_cli_exit_even_if_descendant_holds_output() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & exit 7"]);
        let mut process = Process::spawn(command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while process.running() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            process.child.as_mut().unwrap().wait().unwrap().code(),
            Some(7)
        );
    }
}
