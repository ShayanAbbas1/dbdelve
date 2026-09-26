# SSH tunnels

Postgres, MySQL and SQL Server profiles can connect through a local port
forward run by your system `ssh`, so `~/.ssh/config` (aliases, `ProxyJump`,
`IdentityFile`, `User`), ssh-agent, an agent like 1Password's or Secretive's,
and a hardware key all work exactly as they do in a terminal. DBDelve ships no
SSH client of its own and never reads a private key directly.

## Setting it up

Give the SSH side an entry in `~/.ssh/config` rather than filling every field
in the form — it's the one place `ProxyJump`, a non-default `User` or a
particular `IdentityFile` actually take effect:

```
Host prod-bastion
    HostName bastion.example.com
    User ops
    IdentityFile ~/.ssh/prod_ed25519
    ProxyJump jump.example.com
```

Then, on the profile, turn on **Connect through an SSH tunnel** and fill in:

| Field | |
| --- | --- |
| SSH host | The alias (`prod-bastion` above) or a hostname. |
| SSH port | Optional; blank defers to the config, or 22. |
| SSH username | Optional; blank defers to the config, or the local user. |
| Identity file | Optional; blank defers to the config, or ssh-agent. Absolute, or under `~/`, which `ssh` itself expands — a relative path is refused. |

**Host and Port on the rest of the form are as the SSH host sees them.** A
`localhost` there means the SSH host itself, not your machine — the usual
case is the database's real hostname, or an address only that host's network
can reach.

The tunnel opens when the profile connects and stays open for as long as the
connection does; closing the connection, quitting DBDelve, or a crash all end
the `ssh` process with it, so nothing is left listening behind you.

## What's supported

- Any auth `ssh` can do without prompting: a key already in ssh-agent
  (a hardware key or one held by 1Password's or Secretive's agent included),
  or an unencrypted key file.
- Everything `~/.ssh/config` can say about the host: aliases, `ProxyJump`,
  `User`, `IdentityFile`, `Port`, `IdentitiesOnly`, and the rest.
- TLS still verifies the database's real hostname, not the tunnel's local
  address, on Postgres and SQL Server — `sslmode=verify-full` works through a
  tunnel the same as it does direct.
- A host your `ssh` config says nothing about is trusted on first connection
  (`accept-new`). If your config sets `StrictHostKeyChecking` for that host
  yourself, DBDelve leaves it exactly as you set it.

On Windows, DBDelve uses the `ssh` on your `PATH`, or Windows' built-in
OpenSSH client if none is there. That build's `ssh-agent` service is off by
default — `Set-Service ssh-agent -StartupType Automatic; Start-Service
ssh-agent` before `ssh-add`ing a key, or use an unencrypted key file instead.

## Limits

- **No password auth and no passphrase prompts.** The tunnel runs
  non-interactively: a key that needs a passphrase, or a host that only
  offers password auth, fails rather than prompting. Decrypt the key first
  (see `docs/snowflake.md` for the same `openssl pkcs8` step) or load it into
  ssh-agent unlocked.
- **MySQL's `verify-full` is refused through a tunnel.** The driver checks
  the certificate against the address it actually dials, and through a
  tunnel that's the local forward, not the server — so `verify-full` there
  could only ever check the wrong name. Use `verify-ca` instead, which still
  checks the certificate chain, just not the hostname.
- **No SOCKS or dynamic forwarding.** DBDelve opens one forward to one
  database; it isn't a general-purpose proxy.
- **Not shared between profiles.** Two profiles through the same bastion open
  two separate `ssh` processes.

## Common failures

These are `ssh`'s own messages, shown as they are:

- **`Permission denied (publickey)`** — the SSH host didn't accept any key
  offered. Check the identity file path, and that the right key is loaded
  (`ssh-add -l`); there's no password fallback to try instead.
- **`Host key verification failed`** — the host's key isn't in your
  `known_hosts`, and something in your `~/.ssh/config` set
  `StrictHostKeyChecking` for it explicitly (DBDelve only trusts a new host
  automatically where nothing already decided that). Connect with plain
  `ssh` once to add the key, or leave `StrictHostKeyChecking` unset for that
  host.
- **`Could not resolve hostname …`** — the SSH host or alias itself doesn't
  resolve. Check the SSH host field and your `~/.ssh/config`.
- **`open failed: connect failed: Connection refused`** — the SSH host was
  reached, but nothing is listening at the database's Host/Port *from that
  host's side*. Remember Host and Port are as the SSH host sees them, not as
  your machine sees them.
- **`open failed: connect failed: Name does not resolve`** — the database's
  hostname doesn't resolve from the SSH host, even though the SSH host
  itself did.
- **"… did not come up within 30 seconds."** — `ssh` never finished setting
  up the forward. The usual cause is a hardware key waiting on a touch that
  didn't come, or a slow `ProxyJump`.
