# AWS SSM tunnels

Choose **AWS SSM** under **Tunnel** in the connection form to forward a
private database connection through a Systems Manager managed instance.
DBDelve runs your installed AWS CLI and Session Manager plugin. The database
Host and Port remain the destination as seen from that instance; DBDelve
chooses the local port automatically.

## Before connecting

- Install AWS CLI v2 and the Session Manager plugin on this computer.
- Configure credentials with the AWS CLI. For an SSO profile, run
  `aws sso login --profile your-profile` before connecting.
- Use an SSM-managed instance (`i-…` or `mi-…`) with SSM Agent 3.1.1374.0 or
  later, online in the selected region and reachable through Session Manager.
- The instance must resolve and reach the database's hostname and port.
- Your identity needs permission to start a session on that instance with
  `AWS-StartPortForwardingSessionToRemoteHost`, and to terminate its sessions.
  The instance also needs its usual Systems Manager permissions and network
  access to the service.

See [AWS's remote-host forwarding requirements](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-sessions-start.html#sessions-remote-port-forwarding)
and [plugin installation](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html).

## Connection settings

**Target instance** is required. **AWS profile** and **AWS region** are optional:
blank values let the CLI use its normal credential and region resolution.
Assumed roles, credential processes, environment credentials, and existing SSO
sessions remain the CLI's responsibility. DBDelve does not store AWS keys or
perform an interactive login.

Selecting AWS SSM checks both tools locally in the background. **Found** means
that the executable's `--version` command completed successfully and returned
recognized version output; it does not verify AWS credentials, permissions,
or database connectivity. Each check has a three-second timeout.
**Wrong executable** distinguishes an unrecognized tool from **Missing** or
**Couldn't run**. These states offer **Choose executable…** and an official
**Installation guide**. After installation, use
**Check again**; if your desktop app's PATH has not picked up the installation,
choose the executable under **Advanced** or restart the app.

The optional **AWS CLI path** and **Plugin path** under **Advanced** accept absolute paths, useful
when an app launched from Finder or a desktop launcher has a different PATH
from your shell. The plugin file must be named `session-manager-plugin`
(`session-manager-plugin.exe` on Windows). Paths containing spaces work.
On Windows, a different plugin next to `aws.exe` takes precedence over PATH;
DBDelve refuses that conflict and asks you to select that copy or another CLI.
No shell startup files are loaded. Install the tools separately; DBDelve does
not download them.

**Test** opens the same transport and database handshake as **Connect**, then
closes it. Editing or duplicating a connection preserves its SSM settings.
Choosing another tunnel type keeps the unsaved fields, but saves only the
selected transport. SSH and SSM cannot be combined.

## Encryption and database support

SSM forwards a single TCP destination. Unix sockets, SSH Run as, ECS targets,
custom SSM documents, host lists, and SRV/discovery-based routes are not
supported by this option. File databases do not have a tunnel selector.

The TCP routing paths already used for SSH are used for SSM: PostgreSQL,
MySQL/MariaDB, SQL Server, Redis, ClickHouse, and supported explicit
ODBC endpoints. All their existing tunnel restrictions still apply:

- PostgreSQL, SQL Server and ClickHouse keep the real database hostname for
  TLS verification while dialing the local forward.
- MySQL/MariaDB and Redis refuse `verify-full` through a tunnel because their
  drivers cannot separate the dial address from the verification name.
- MongoDB SSM is refused: its driver opens background connections without a
  hook to recheck tunnel liveness. Its existing SSH support is unchanged.
- ODBC requires an explicit endpoint that DBDelve can safely rewrite. DSNs
  and unsupported/ambiguous connection strings are refused. TLS remains the
  driver's responsibility; configure its separate certificate hostname where
  available, otherwise a hostname mismatch fails.

No TLS mode is silently weakened. Password commands, including RDS IAM token
commands, still receive the original database host and port.

## Failures and cleanup

Missing tools, expired credentials/SSO, access denial, and an offline target
produce an actionable error. Startup has a 30-second limit. Unknown CLI
output fails closed; raw tracebacks are not shown because they can contain
session tokens. A database handshake still has to succeed after the local
listener is ready.

Closing or retesting the form cancels the old attempt. Disconnecting ends the
local CLI/plugin process tree. On Unix a watchdog survives the app and uses
its lifetime pipe to detect even an app crash; on Windows it owns a kill-on-close
Job Object. Local cleanup is independent of a bounded, best-effort remote
`terminate-session` call. Abrupt crashes or lost network access may leave the
remote session visible until AWS expires it. A dropped tunnel never falls
back to a direct database connection or replays a query.

**Downgrading:** SSM profiles are saved with a non-string engine guard
(`[profiles.engine.ssm]`) as well as their tunnel settings. Older releases
cannot read that engine as a direct connection. Releases with unavailable-profile
support retain the whole profile without opening it; earlier releases, or the
plain-reader fallback, reject the file and may preserve it as
`profiles.toml.broken`. Keep a backup or use a separate application variant
when testing older versions, since those versions cannot use the SSM profile.
Existing direct and SSH profiles retain their original format.

SSM profiles from earlier development builds acquire the guard when saved by
this build. An old copy that has never been saved by the fixed build remains
unsafe to open in an older release. A guarded profile with missing tunnel
settings is refused, never restored as a direct connection.

## Opt-in live verification

Normal tests use fake child processes and loopback sockets, without AWS
credentials. A live PostgreSQL integration test is ignored by default:

```sh
export DBDELVE_SSM_POSTGRES_URL='postgresql://user:password@private-db.example/db?sslmode=verify-full'
export DBDELVE_SSM_TARGET='i-0123456789abcdef0'
export DBDELVE_SSM_PROFILE='your-profile'
export DBDELVE_SSM_REGION='your-region'
cargo test live_ssm_query_side_connection_cancel_and_cleanup -- --ignored
```

Optional `DBDELVE_SSM_AWS_CLI` and `DBDELVE_SSM_PLUGIN` select absolute tool
paths. Use a disposable test database and a role permitted to query it. This
checks a query, a secondary connection, cancellation, and listener cleanup;
`verify-full` and a trusted server certificate also exercise TLS identity.
