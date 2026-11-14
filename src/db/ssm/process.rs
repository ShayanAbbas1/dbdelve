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
    #[cfg(windows)]
    let job = {
        use std::os::windows::io::BorrowedHandle;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // Join before spawning: Windows only enrolls children born after the
        // parent joins a job. Closing this job ends the watchdog too, which
        // owns no resources that need to survive tunnel teardown.
        // SAFETY: the current-process pseudo handle is valid for this borrow
        // and is never closed by BorrowedHandle.
        let process = unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) };
        super::super::ssh::kill_on_close_job(&process)?
    };
    let mut child = crate::db::spawn(&mut command)?;
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::{
        io::BufRead,
        net::TcpListener,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        sync::mpsc,
    };
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::{
            JobObjects::{JobObjectBasicProcessIdList, QueryInformationJobObject},
            Threading::{
                OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
                WaitForSingleObject,
            },
        },
    };

    const FIXTURE: &str = "DBDELVE_TEST_WINDOWS_SSM";
    const TEST: &str = "db::ssm::process::windows_tests::process_fixture";

    fn fixture() -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST, "--nocapture"])
            .env(FIXTURE, "cli");
        command
    }

    // Runs only in explicitly launched subprocesses, never in the test runner.
    #[test]
    fn process_fixture() {
        let Ok(role) = std::env::var(FIXTURE) else {
            return;
        };
        if std::env::var_os(COMMAND).is_some() {
            let program = std::env::current_exe().unwrap();
            let pid = std::process::id().to_string();
            let args = ["--exact", TEST, "--nocapture", "--skip", &pid];
            let payload = serde_json::to_string(&(program, args)).unwrap();
            std::process::exit(run(&payload).unwrap());
        }
        if role == "plugin" {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            println!(
                "SSM_TEST_READY {} {}",
                std::process::id(),
                listener.local_addr().unwrap().port()
            );
            io::stdout().flush().unwrap();
            // Bound even on a broken cleanup path, so a failed test leaves no
            // permanent listener behind.
            thread::sleep(Duration::from_secs(15));
            drop(listener);
            std::process::exit(0);
        }
        let watchdog: usize = std::env::args().next_back().unwrap().parse().unwrap();
        // Check before creating descendants, then again after the old
        // spawn-then-assign implementation would have moved us to its job.
        for delay in [Duration::ZERO, Duration::from_millis(250)] {
            thread::sleep(delay);
            #[repr(C)]
            struct Members {
                assigned: u32,
                count: u32,
                ids: [usize; 16],
            }
            let mut members = Members {
                assigned: 0,
                count: 0,
                ids: [0; 16],
            };
            // SAFETY: null queries this process's immediate job. The buffer
            // has the documented variable-length process-ID-list layout.
            assert_ne!(
                unsafe {
                    QueryInformationJobObject(
                        std::ptr::null_mut(),
                        JobObjectBasicProcessIdList,
                        (&raw mut members).cast(),
                        size_of_val(&members) as u32,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            assert!(
                members.ids[..members.count as usize].contains(&watchdog),
                "CLI must inherit the watchdog's job before it runs"
            );
        }
        let mut plugin = fixture();
        plugin.env(FIXTURE, "plugin");
        crate::db::spawn(&mut plugin).unwrap().wait().unwrap();
        std::process::exit(0);
    }

    #[test]
    fn watchdog_job_is_inherited_and_disconnect_ends_plugin() {
        let mut command = fixture();
        // The CLI inherits this override through run; only the watchdog test
        // dispatch uses COMMAND, which run removes before spawning the CLI.
        command
            .env(FIXTURE, "cli")
            .env(COMMAND, "test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut process = Process {
            child: Some(crate::db::spawn(&mut command).unwrap()),
        };
        let stdout = process.child.as_mut().unwrap().stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((_, ready)) = line.split_once("SSM_TEST_READY ") {
                    let _ = send.send(ready.to_owned());
                    break;
                }
            }
        });
        let ready = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("plugin readiness");
        let (pid, port) = ready.split_once(' ').unwrap();
        // Hold a real handle so PID reuse cannot make the exit check pass.
        let raw = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid.parse().unwrap(),
            )
        };
        assert!(!raw.is_null());
        let plugin = unsafe { OwnedHandle::from_raw_handle(raw) };
        process.stop();
        let exited = unsafe { WaitForSingleObject(plugin.as_raw_handle(), 5000) };
        if exited != WAIT_OBJECT_0 {
            unsafe {
                TerminateProcess(plugin.as_raw_handle(), 1);
            }
        }
        assert_eq!(exited, WAIT_OBJECT_0, "plugin survived disconnect");
        assert!(
            TcpListener::bind(format!("127.0.0.1:{port}")).is_ok(),
            "listener survived disconnect"
        );
    }
}
