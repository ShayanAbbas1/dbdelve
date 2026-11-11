# SSH tunnels

`src/db/ssh.rs` opens a local port forward over the *system* `ssh` binary,
never a library, and every engine dials through it (`Tunnel::dial`) before
connecting. That is deliberate: a library would need its own config parser,
its own agent protocol and its own hardware-key support to match what
invoking `ssh` gets for nothing — `~/.ssh/config` (aliases, `ProxyJump`,
`IdentityFile`, `User`), ssh-agent, and whatever agent or hardware key is
already set up on the machine.

- **The dialled address and the name TLS verifies are never the same field,
  except on MySQL, where the driver has no other field to give it.** Postgres
  passes the tunnel's loopback address as libpq's `hostaddr` beside
  `host=<server.host>` (`postgres::tunnelled_string`), so the driver dials the
  forward and still checks the certificate against the real name. SQL
  Server's `login` connects the raw `TcpStream` to the tunnel's address
  directly but still builds `tiberius::Config` with `config.host(&server.host)`,
  so `Client::connect`'s handshake checks the real name. **MySQL's
  `OptsBuilder::ip_or_hostname` is both the dial target and the name its own
  domain check runs against** (`mysql::base_options`), so through a tunnel it
  can only ever check the tunnel's own address. That is exactly why MySQL
  refuses `sslmode=verify-full` through a tunnel outright
  (`unverifiable_through_a_tunnel`) rather than silently checking the wrong
  name, as Redis (`Connection::open`) and MongoDB
  (`unreachable_through_a_tunnel`, which refuses `verify-ca` too, since its
  driver checks the name there as well) do for the same reason, and why `base_options` sets `prefer_socket(false)` when dialling
  through a tunnel: a loopback host would otherwise have the driver ask the
  server for its Unix socket path and
  reconnect to *that* path on this machine, past the tunnel entirely. Nowhere
  does `server.host` get rewritten to `127.0.0.1` to make a driver happy; the
  places above are the only ones a tunnel's address reaches.
  ClickHouse's `ureq` agent takes a resolver (`TunnelResolver`) that dials the
  tunnel for every socket it opens and returns the loopback address, while the
  URI keeps the real host, so SNI and the hostname check use the real name and
  `verify-full` works through a tunnel.
- **Readiness is read off `ssh -v`'s own lines, not a TCP probe.** `-v`'s
  `debug1: Local forwarding listening on 127.0.0.1 port <port>.` fires once
  the listener is bound, but ssh prints it before the session behind it is
  actually up — connecting the moment the socket exists can land before ssh
  is ready to service it. `Tunnel::open` waits for that line and *then*
  `debug1: Entering interactive session.`, which `ExitOnForwardFailure=yes`
  guarantees never appears if any forward failed to bind. Both together, not
  either alone, are the ready signal (`Said::ready`).
- **The watchdog exists because `std::process::Command` hands a child the
  spawning thread's signal mask, and a GCD worker thread blocks every
  signal.** A connect runs on the background executor, one of those workers
  on macOS; ssh spawned there inherits a mask that blocks `SIGTERM`, so an
  ordinary kill would be ignored and ssh would outlive DBDelve. The Unix
  watchdog (`WATCHDOG` in `ssh.rs`) is a shell holding a pipe's read end on
  its own stdin, whose write end only DBDelve holds; the shell's `pre_exec`
  clears the *child's* mask to empty and puts `SIGTERM` back to its default
  disposition first, so the signal reaches ssh however DBDelve exits, not
  just on a clean `Drop`. `live_ssh_a_tunnel_opened_with_every_signal_blocked_still_ends_on_drop`
  is the regression test.
- **Windows has no watchdog shell, so it kills the tree through a Job
  Object** (`kill_on_close_job`), created with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assigned right after spawn. Marked
  `ponytail:` in the code: ssh runs briefly before it joins the job, so a
  DBDelve killed in that window can leave it behind; spawning suspended and
  resuming only after assignment would close that gap, if it's ever worth
  doing.
- **`ControlMaster=no` and `ControlPath=none` are load-bearing, not
  cleanliness.** A config's `ControlPersist` would fork ssh into the
  background past `Drop` and the watchdog, and an existing control master
  would take the forward over from a process DBDelve never held a handle to
  — either way the tunnel would outlive the connection.
- **An effective `ask` becomes `accept-new`.** `host_key_checking_is_ask`
  runs `ssh -G` over the same arguments. Its output does not distinguish
  default `ask` from explicit `ask`, so both receive the override. Other
  values are preserved; if `ssh -G` fails, no override is added.
- **`Tunnel::dial` re-checks the process before every use** — every fresh
  connect, every cancel's second socket (Postgres's `CancelToken`, MySQL's
  `KILL QUERY` connection), every SQL Server reconnect — with `try_wait()`
  for `Ok(None)` before handing back the local address, since a port a dead
  ssh released is one anything else on the machine could have bound since. A
  dead tunnel fails the dial with what ssh last said on its stderr, rather
  than reaching whatever is now listening on that port.

- **A key passphrase is answered through ssh's askpass, never a prompt or a
  terminal** (`ssh/askpass.rs`). It is a secret, so it lives where the
  password does: a second Keychain item per profile, under the profile id
  like the password but in a service of its own (`<variant>/ssh-passphrase`,
  `store::Secret`), so no hand-edited profile id can name another profile's
  passphrase as its password, and no variant's name, which may not hold a
  `/`, can spell the service. It sits on `SshTunnel` with
  `#[serde(skip)]`, is filled in at connect and Test, carried by Duplicate
  and removed with the profile (`store::delete_secrets`). Blank is none, and then
  ssh runs exactly as without the field, `BatchMode=yes` and all.
  - **A passphrase drops `BatchMode`, because `BatchMode` blocks askpass**
    even under `SSH_ASKPASS_REQUIRE=force`. In its place go `BatchMode=no`
    (a config's `BatchMode yes` would block it too, and the command line
    wins), `PasswordAuthentication=no` and `KbdInteractiveAuthentication=no`,
    so the only prompt left is the key's own. ssh runs with `SSH_ASKPASS` set to
    DBDelve's own executable and `SSH_ASKPASS_REQUIRE=force`; `main` answers
    as askpass before anything else starts when `DBDELVE_ASKPASS` is set.
  - **The passphrase never enters ssh's environment or arguments**, which
    last as long as ssh runs. While a session opens, DBDelve serves it on a
    loopback port to whoever shows a 128-bit one-time token
    (`DBDELVE_ASKPASS`, `DBDELVE_ASKPASS_TOKEN`), compared in constant time;
    a wrong token is closed on without an answer. TCP rather than a Unix
    socket, so one path serves Windows. The listener lives until the session
    is ready or ssh exits, since ssh asks again after a wrong passphrase, and
    the token is useless after that. Each exchange gets one deadline for the
    whole of it, so a client trickling bytes cannot hold up the next, and
    dropping the listener never waits on one. A `SocketBridge` holds the
    passphrase and serves it again for every login.
  - **The token is in ssh's environment, and that is accepted.** A process of
    the same user that reads it while the session opens can fetch the
    passphrase: the same-user boundary ssh-agent's socket has, not one this
    defends.
  - **Only a prompt starting with `Enter passphrase for key` and a space is
    answered.** A host key question, a server's password or
    keyboard-interactive prompt and a hardware key's PIN exit non-zero
    without connecting, so the passphrase is never sent as a password. A `ProxyJump` hop is its own ssh without our `-o` options, so it
    is this filter, not the options, that keeps the passphrase from it, and
    it holds because OpenSSH 8.5 begins a server's keyboard-interactive
    prompt with `(user@host)`, so no server can spell the prefix.
  - **A passphrase is refused on an ssh older than OpenSSH 8.5**, naming it,
    from `ssh -V` (read once per process, and only when a passphrase is set;
    `OpenSSH_for_Windows_` counts). Before 8.4 ssh ignores
    `SSH_ASKPASS_REQUIRE`; 8.4 honours it but still leaves a server's
    keyboard-interactive prompt unprefixed (the `(user@host)` prefix is
    8.5's), so a hop's server could ask for the passphrase as if it were ssh.
    An unreadable version is refused too: never a quiet fallback.
  - **A refused login says what a passphrase has to do with it.** With none
    given, after a key the SSH host accepted (`-v`'s `Server accepts key:`)
    or with Identity file set, the error suggests entering it or ssh-agent;
    with one given that ssh was handed, that it may be wrong.
  - In the tests `current_exe()` is the test binary, whose arguments libtest
    would read as a filter, so a `cfg(test)` loader hook (`.init_array`,
    `__mod_init_func`) answers as askpass before libtest's main. It needs the
    loader to pass it argv, which glibc and macOS do and musl does not, so
    the hook and the tests that lean on it are glibc and macOS only. The live
    tests use the `dbdelve-bastion-keyless` entry `dev/ssh/setup.sh` writes,
    with no identity or agent of its own, and an encrypted copy of the key.

- **An ODBC profile tunnels by rewriting its connection string at open, and
  refuses whenever it cannot be sure the driver dials only what it rewrote.**
  Its address is inside the string, in driver-specific keys, so
  `odbc_string::endpoint` finds exactly one host attribute (`HOSTNAME`,
  `HOST`, `SERVER`, `SERVERNAME`, `ADDRESS`, `ADDR`) and one port, the tunnel
  forwards to them, and `through_tunnel` points them at the forward. Where the
  port goes depends on the driver, so the string must name one `DRIVER` DBDelve
  knows: Microsoft's SQL Server drivers read it only after a comma in the host
  (`Server=host,port`) and ignore a `PORT` key, psqlODBC, MySQL's, MariaDB's and
  Db2's read a `PORT` key and take a comma as a host list. An unknown driver, a
  DSN (its address is in `odbc.ini`, out of reach), a second host or port, no
  port (no default is guessed: `Server=` alone means 5432, 3306 or 1433 by
  driver), a host that is not one plain name (IPv6, `host:port`, a named
  instance, a protocol prefix other than `tcp:`), a braced value a driver could
  read differently, and any key that may name another address (`DBQ`,
  `SOCKET`, `PQOPT`, failover partners, alternate servers, proxies: a denylist
  that leans wide) are each refused by name. A refusal costs a connect; a guess
  sends the driver, and the password, past the tunnel. `ponytail:` the major
  drivers' spellings only; one more is a line in the key lists.
- **The rewrite exists only in the `open` request.** The stored string stays
  the user's; the rewritten one is never persisted, logged or shown. It changes
  exactly the host and port values (to `127.0.0.1` and the forward's port, a
  `tcp:` prefix and braces kept), and is parsed again and refused ("This
  connection string could not be routed through the tunnel.") unless it names
  exactly the forward with every other attribute identical, in order; the
  rewrite and the check are one function, so no caller gets one without the
  other. The form shows what the tunnel will do under the Connection field, and
  `open` checks again for a hand-edited profile. `apart` dials the same tunnel.
- **Through a tunnel an ODBC driver verifies `127.0.0.1`**, because DBDelve
  sets no TLS on an ODBC profile and rewrites none of the driver's TLS keys. A
  driver that checks the hostname fails loudly with its own certificate error,
  which weakens nothing (hard rule 6): DBDelve asked for no mode it could
  downgrade. A driver with a separate verification name (SQL Server ODBC 18's
  `HostNameInCertificate`) can be given one by the user; Kerberos, which derives
  the SPN from the host, fails the same loud way.

## Unix socket bridge

A Postgres profile whose Host starts with `/` and has an SSH tunnel reaches a
Unix socket on the SSH host (`ssh::SocketBridge`, `postgres::Route::Socket`).
It is not a forward.

- **The stream is a private loopback socket pair with a peer check, never a
  listener.** With peer authentication the server trusts whoever opens the
  socket, so a port anyone on this machine could dial would hand them that
  login: the bridge is the authentication. `socket_pair` binds a listener only
  to connect to it once and accepts only the connection whose address is that
  client's own, dropping any other. ssh's stdio is copied to and from that
  pair (a pair rather than a socket handed to the child, because Windows
  cannot) by a read and write loop, never `io::copy`: on Linux that splices,
  and a blocking splice from a socket into a pipe holds the pipe's lock while
  it waits, so ssh never reads the startup message (`pump`).
- **A `/` Host is the socket directory, as in libpq, and Port picks the
  file** (`<dir>/.s.PGSQL.<port>`). `Run as` is the user `sudo -n -u` opens it
  as; blank is the SSH user. `-n` because with no terminal on the SSH
  host a sudo that wants a password must fail saying so rather than wait for
  one nobody can type. A set
  Run as with a non-socket Host, or on a forward, is refused at connect for
  every engine rather than quietly connecting as the SSH user
  (`ssh_socket_error`, `ssh_args`), which is what catches a hand-edited
  profile. The form shows Run as only on a target that reaches a socket, drops
  a hidden one on save, and there refuses a non-socket Host on Save and Test.
- **Run as and the path are validated to a narrow character set, not
  quoted.** They reach the remote login shell, and the path's `:`, `,` and `!`
  reach socat's address parser a second time, so quoting would have to be right
  for both grammars. `socket_bridge_error` admits letters, digits, `.`, `_`,
  `-` (and `/` in the path, an absolute one; a user name cannot start with
  `-`). `ponytail:` a space or comma in a path is refused; quoting for both is
  the upgrade.
- **The watchdog's pipe moves to fd 3, because stdin and stdout are the
  data** (`BRIDGE_WATCHDOG`, `watched_bridge`). EOF on the data cannot stand
  in for it: ssh reads no stdin until it has logged in, which can wait on a
  hardware key's touch, so a DBDelve that died then would leave ssh behind.
  On drop, `Bridged` does not kill at once: `cancel_query_raw` writes and returns without waiting, so a
  kill would lose a cancel (or a Terminate) still in flight. A thread waits up
  to `CLOSED_WITHIN` for ssh to exit on its own, then `Process`'s drop kills
  it; nothing blocks on drop. Windows keeps the Job Object.
- **Readiness is `-v`'s `debug1: Sending command: `, not `Entering
  interactive session.`** A `ProxyJump` hop prints the latter before the last
  login. `connect` waits for it (`READY_WITHIN`, long enough for a hardware-key
  touch) before handing out the stream, so auth, DNS and host-key failures
  read `SSH bridge through X failed: ...` like a tunnel's and the driver's
  handshake keeps its own 10 s. sudo's or socat's refusal arrives either before
  that wait ends or, racing it, appended after a failed handshake by
  `Bridged::explain`. Exit status 127 means no local ssh only before
  `Sending command`; after it, it is the SSH host's "command not found" for
  socat or sudo.
- **The driver gets `NoTls`, and `require`, `verify-ca` and `verify-full` are
  refused by name** (hard rule 6): Postgres offers TLS only over TCP, and SSH
  already encrypts to the SSH host. `disable` and `prefer` connect; `prefer`
  over a connector that cannot do TLS sends no SSLRequest, as libpq does on a
  socket (`prefer_over_a_bridge_opens_with_the_startup_message_not_an_ssl_request`).
- **Every connection is its own ssh login** (the session, the side session,
  sizes, each cancel), since `ControlMaster=no` stays load-bearing.
  `ponytail:` the upgrade is a DBDelve-owned master (`-M -S <private path>
  -o ControlPersist=no`, watched like a tunnel) the bridges multiplex over;
  Windows' OpenSSH cannot multiplex.
