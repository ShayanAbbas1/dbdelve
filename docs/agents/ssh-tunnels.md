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
