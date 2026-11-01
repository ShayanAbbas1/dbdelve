//! The built bridge, driven over its real pipes.

use std::collections::BTreeMap;
use std::env;
use std::io::Write;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use dbdelve_plugin_protocol::{
    Kind, PROTOCOL, Password, Request, Response, read_frame, write_frame,
};

fn bridge() -> Child {
    Command::new(env!("CARGO_BIN_EXE_dbdelve-odbc"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn send(child: &mut Child, id: u64, request: Request) {
    let stdin = child.stdin.as_mut().unwrap();
    write_frame(stdin, Kind::Control, id, &request.to_payload()).unwrap();
    stdin.flush().unwrap();
}

fn receive(stdout: &mut ChildStdout) -> (u64, Response) {
    let frame = read_frame(stdout).unwrap().expect("a frame before EOF");
    assert_eq!(frame.kind, Kind::Control);
    (
        frame.request,
        Response::from_payload(&frame.payload).unwrap(),
    )
}

fn exits_within(child: &mut Child, deadline: Duration) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    None
}

#[test]
fn it_exits_on_stdin_eof() {
    let mut child = bridge();
    let mut stdout = child.stdout.take().unwrap();
    send(&mut child, 1, Request::Hello { protocol: PROTOCOL });
    assert!(matches!(receive(&mut stdout), (1, Response::Hello { .. })));
    drop(child.stdin.take());
    assert_eq!(exits_within(&mut child, Duration::from_secs(5)), Some(0));
}

/// `dbdelve_ODBC_DB2` is a connection string without credentials, such as
/// `DRIVER=/abs/path/libdb2.dylib;HOSTNAME=127.0.0.1;PORT=55000;DATABASE=dbdelve;PROTOCOL=TCPIP`;
/// the user and password travel in their own fields, as the app sends them.
#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2, _USER and _PASSWORD"]
fn live_open_names_the_dbms_and_close_ends_the_process() {
    let connection = env::var("dbdelve_ODBC_DB2").expect("dbdelve_ODBC_DB2");
    let user = env::var("dbdelve_ODBC_DB2_USER").unwrap_or_default();
    let password = env::var("dbdelve_ODBC_DB2_PASSWORD").unwrap_or_default();
    let mut child = bridge();
    let mut stdout = child.stdout.take().unwrap();
    let fields = BTreeMap::from([
        ("connection".to_string(), connection),
        ("user".to_string(), user),
    ]);
    send(
        &mut child,
        1,
        Request::Open {
            fields,
            password: Password(password),
            statement_timeout: 0,
        },
    );
    let (id, opened) = receive(&mut stdout);
    assert_eq!(id, 1);
    let Response::Opened {
        dbms_name,
        dbms_version,
        driver,
        stops_early,
        ..
    } = opened
    else {
        panic!("opened, got {opened:?}");
    };
    eprintln!("opened: {dbms_name} {dbms_version} through {driver}");
    assert!(dbms_name.starts_with("DB2/"), "{dbms_name}");
    assert!(!dbms_version.is_empty());
    assert!(!driver.is_empty());
    assert!(!stops_early);

    send(&mut child, 2, Request::Close);
    assert_eq!(receive(&mut stdout), (2, Response::Closed));
    assert_eq!(exits_within(&mut child, Duration::from_secs(5)), Some(0));
}

#[test]
#[ignore = "needs a Db2 and IBM's driver: set dbdelve_ODBC_DB2"]
fn live_a_wrong_password_is_the_drivers_error() {
    let connection = env::var("dbdelve_ODBC_DB2").expect("dbdelve_ODBC_DB2");
    let user = env::var("dbdelve_ODBC_DB2_USER").unwrap_or_default();
    let mut child = bridge();
    let mut stdout = child.stdout.take().unwrap();
    let fields = BTreeMap::from([
        ("connection".to_string(), connection),
        ("user".to_string(), user),
    ]);
    let password = "not-the-password;}".to_string();
    send(
        &mut child,
        1,
        Request::Open {
            fields,
            password: Password(password.clone()),
            statement_timeout: 0,
        },
    );
    let (_, error) = receive(&mut stdout);
    let Response::Error {
        message, sqlstate, ..
    } = error
    else {
        panic!("an error, got {error:?}");
    };
    eprintln!("refused: {sqlstate:?} {message}");
    assert!(sqlstate.is_some());
    assert!(!message.contains(&password));
}
