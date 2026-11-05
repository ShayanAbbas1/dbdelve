# Connecting through ODBC

DBDelve has built-in engines for Postgres, MySQL, MariaDB, SQLite, Turso,
DuckDB, Snowflake, SQL Server, MongoDB, Redis and ClickHouse. Any other
database with an ODBC driver (IBM Db2, Oracle, Informix, Teradata, Firebird and
so on) can be reached through the ODBC bridge that ships inside DBDelve.

DBDelve brings the bridge. **You bring the database's ODBC driver**, the same
one any other ODBC tool would use.

## 1. Install the driver

Get the ODBC driver from your database vendor.

| | What you need |
| --- | --- |
| **macOS** | The vendor's driver, built for your Mac: an Apple silicon Mac needs an arm64 (or universal) driver. Nothing else; DBDelve has the ODBC driver manager built in. |
| **Linux** | The `unixodbc` package (`sudo apt install unixodbc`, `sudo dnf install unixODBC`, `sudo pacman -S unixodbc`) and the vendor's driver. |
| **Windows** | The vendor's installer. Windows has its own ODBC manager. Windows support is new and has had the least testing. |

Note where the driver's library file ends up, for example
`/opt/ibm/clidriver/lib/libdb2.dylib` or `/usr/lib/x86_64-linux-gnu/odbc/psqlodbcw.so`.
You'll need it in the next step.

## 2. Write the connection string

The connection string tells the driver what to connect to. It's a list of
`KEY=value` pairs separated by `;`.

The most reliable form names the driver library by its full path:

```
DRIVER=/opt/ibm/clidriver/lib/libdb2.dylib;HOSTNAME=db.example.com;PORT=50000;DATABASE=sales;PROTOCOL=TCPIP
```

The keys after `DRIVER=` belong to your driver, not to DBDelve, so check its
documentation. Some common ones:

| Driver | Host | Port | Database |
| --- | --- | --- | --- |
| IBM Db2 | `HOSTNAME` | `PORT` | `DATABASE` (plus `PROTOCOL=TCPIP`) |
| psqlODBC (Postgres) | `Server` | `Port` | `Database` |
| MySQL / MariaDB Connector/ODBC | `Server` | `Port` | `Database` |
| Microsoft ODBC Driver for SQL Server | `Server=host,port` | | `Database` |

Leave the user name and password out of the string. They have their own fields
in the form (step 3).

### Using a DSN instead

If you already have a data source set up, the connection string can be just
`DSN=SalesDb`.

- **Windows:** create it in *ODBC Data Sources (64-bit)*.
- **Linux:** register the driver in `/etc/odbcinst.ini` and the DSN in
  `/etc/odbc.ini` or `~/.odbc.ini`.
- **macOS:** DBDelve's built-in driver manager reads `/etc/odbcinst.ini`,
  `/etc/odbc.ini` and `~/.odbc.ini`. It does **not** read Homebrew's
  `odbcinst.ini` or `/Library/ODBC/`, where some vendor installers register
  their drivers. If a driver name from one of those isn't found, put the
  library's full path in `DRIVER=` instead, or start DBDelve with `ODBCSYSINI`
  set to the folder holding an `odbcinst.ini` that names it.

A DSN can't be used with an SSH tunnel, because it hides the host and port.

## 3. Add the connection

1. Open a new connection and pick the **ODBC** tile.
2. Paste the connection string into **Connection**.
3. Fill in **User** and **Password**. The password is kept in your system's
   keychain. A password typed into the string itself (`PWD=`, `Password=`, …)
   is used to connect but never saved.
4. Check **SQL dialect**. DBDelve suggests one from the database name the
   driver reports. A database DBDelve knows (Postgres, MySQL, SQL Server, …)
   gets its own dialect; anything else gets **Generic SQL**. The dialect
   decides how DBDelve quotes names and pages through results.
5. Optionally turn on **SSH tunnel** to reach the database through a bastion
   host. See [SSH tunnels](ssh-tunnels.md). Tunnels work for drivers whose
   host and port keys DBDelve recognises: Db2, psqlODBC, MySQL/MariaDB and
   SQL Server.
6. Press **Test**, then **Connect**.

## What works

- Browsing schemas, tables, views and their structure.
- Running queries, with Cancel and the row limit.
- Paging, sorting by column header, filters, row counts and previews.
- Exporting results and whole tables.

## What's different from a built-in engine

- **The grid is read-only.** Cell edits, New Row, Delete Row and Import Rows
  are off for ODBC connections. To change data, set **Access** to Read-write
  and run `INSERT`, `UPDATE` or `DELETE` in the query editor.
- **New ODBC connections start in Read-only.** With Generic SQL, the server
  isn't asked to enforce Read-only, so DBDelve checks each statement itself.
  It refuses anything it can't prove is a read, including calls to functions
  it doesn't know are safe. A query that calls one of your own functions needs
  Read-write.
- **Every statement commits as it runs.** A `BEGIN … COMMIT` typed across
  several statements isn't supported yet.
- **A `;` can't appear in the user name or password.**
- **psqlODBC changes some dates and timestamps** on the way through: BC dates,
  `infinity`, years past 9999 and `timestamptz` offsets. Cast them to `::text`,
  or use DBDelve's built-in Postgres engine.
- **Db2 `BEGIN … END` blocks** are split at their inner semicolons under
  Generic SQL.

## When it doesn't connect

| You see | What to do |
| --- | --- |
| `IM002` / "Data source name not found" | The driver or DSN name isn't registered where DBDelve looks. On macOS, use the full library path in `DRIVER=` (see [Using a DSN instead](#using-a-dsn-instead)). |
| `IM003` / the driver can't be loaded | The path is wrong, or the driver is built for the other architecture (an Intel driver on an Apple silicon Mac). Check the path and get the matching build. |
| macOS says the driver library "cannot be opened" | macOS has quarantined the downloaded driver. Clear it: `xattr -dr com.apple.quarantine /path/to/driver-folder`. |
| Linux: the ODBC plugin fails to start | Install `unixodbc`. |
| A user name or password error (Db2: `SQL30082N … reason "24"`) | The credentials are wrong. Db2 logs in as an operating-system user on the server, which is often not the database's name. |
| `08001` or a timeout | DBDelve reached the driver, but the driver couldn't reach the server. Check the host, the port and any firewall, or use an SSH tunnel. |
| The connection drops and **Reconnect** appears | The server or network went away. Reconnect opens a new session; anything held by the old one (temporary tables, session settings) is gone. |
