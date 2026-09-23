# Snowflake

DBDelve talks to Snowflake over its SQL REST API and signs in with a key pair.
This page is what you need to know before filling in the form, and what does
not work yet.

## Signing in

Key-pair authentication only. There is no password, OAuth, browser SSO or
access-token sign-in.

The private key has to be:

- **RSA**, 2048 bits or more.
- **Unencrypted.** A key protected by a passphrase is refused with "is
  encrypted". To make an unencrypted copy of one you already have:

  ```sh
  openssl pkcs8 -topk8 -nocrypt -in rsa_key.p8 -out rsa_key_plain.p8
  ```

- **PEM**, either PKCS#8 (`BEGIN PRIVATE KEY`, what Snowflake's own
  instructions produce) or PKCS#1 (`BEGIN RSA PRIVATE KEY`).

To make a new pair and register it:

```sh
openssl genrsa 2048 | openssl pkcs8 -topk8 -nocrypt -out snowflake.p8
openssl rsa -in snowflake.p8 -pubout -out snowflake.pub
chmod 600 snowflake.p8
```

```sql
ALTER USER my_user SET RSA_PUBLIC_KEY='MIIBIjANBgkq...';  -- the body of snowflake.pub, without its BEGIN/END lines
```

The form takes the key as a file, and the path has to be **absolute**, such as
`/Users/me/.ssh/snowflake.p8`. A relative path or one starting with `~` is
refused: DBDelve does not expand `~`, and an app opened from Finder has no
working directory a relative path could mean anything against. The profile
stores the path and nothing else; the key is read from the file each time it
is needed and never copied anywhere.

If your key lives in a secrets store as one base64 string, write it to a file
first:

```sh
echo 'PASTE_THE_STRING_HERE' | base64 -d > ~/.ssh/snowflake.p8
chmod 600 ~/.ssh/snowflake.p8
```

## The form

| Field | |
| --- | --- |
| Account | The account identifier, such as `myorg-myaccount`. Pasting the sign-in URL works; the identifier is taken out of it. |
| Username | The Snowflake user the public key is registered on. |
| Private key file | Absolute path to the unencrypted key. |
| Database | Required. A profile browses one database, as a Postgres connection does. |
| Warehouse | Optional; blank uses the user's default. **Needed in practice**: see below. |
| Role | Optional; blank uses the user's default. |
| Host | Optional. Only for privatelink, a regional domain or a proxy. Blank means `<account>.snowflakecomputing.com`. |
| Statement timeout | Seconds, sent with each request. Blank is the account's own limit. |

## Things that behave differently here

- **The explorer fills in twice.** Tables and views appear first; functions and
  procedures follow a few seconds later. Listing routines is the slowest thing
  Snowflake's catalog is asked for, and it is not worth waiting on.
- **Connecting resumes the warehouse.** The catalog is read from
  `INFORMATION_SCHEMA`, which needs a running warehouse, and so does opening a
  Structure tab. With no warehouse at all the connection opens and the catalog
  fails with Snowflake's own message.
- **There is no session.** Each run is a separate request, so nothing set in
  one run carries over to the next, an open transaction included. `USE` is
  refused by Snowflake itself ("Command not supported by SQL API: USE").
  Unqualified names resolve against the profile's database, so write
  `MY_SCHEMA.MY_TABLE`.
- **Results are read-only.** Snowflake does not enforce primary keys and does
  not say which table a result column came from, so there is no row DBDelve can
  safely name. Inserting a row from a table's tab works.
- **Keys show only if they are declared.** Most pipelines never declare them;
  dbt only does under an enforced contract.
- **A filter on a `BINARY` column fails**, including following a foreign key
  whose columns are binary.
- **Explain is not offered.**
- **Several statements in one run** show the last statement's result.
- **`TIMESTAMP_LTZ` is shown in UTC**, marked `Z`.
- A whole result is fetched before any of it is shown, so a very large one
  costs memory and time in proportion.
