# Day-1 MariaDB/MySQL Spike — Outcome

Run: 2026-05-19, against maintainer's Apple Silicon machine.

## What's actually on disk

| Path | Real? | Flavor | Version |
|------|------:|--------|---------|
| `~/Library/Application Support/Herd/bin/mysqld` | **NO — dangling symlink** | (would be MySQL 9.7.0) | — |
| `~/Library/Application Support/Herd/bin/mariadbd` | yes | MariaDB | 11.8.6 |
| `/opt/homebrew/opt/mysql/bin/mysqld` | no | — | — |
| `/opt/homebrew/opt/mariadb/bin/mariadbd` | no | — | — |
| `/usr/local/opt/.../bin/{mysqld,mariadbd}` | no (Apple Silicon host) | — | — |

Real MariaDB 11.8.6 install root: `/Users/Shared/Herd/services/mariadb/11.8.6/`.

## Init flow — locked for Block D

- **MariaDB 11.x does NOT accept `--initialize-insecure`.** Confirmed by spike:
  `mariadbd --initialize-insecure ...` exits non-zero with
  `[ERROR] mariadbd: unknown option '--initialize-insecure'`.
- **Canonical MariaDB init**:
  `<basedir>/scripts/mariadb-install-db --datadir=<data> --basedir=<basedir> --auth-root-authentication-method=normal`
  Spike confirmed this creates `mysql`, `performance_schema`, `sys`, `test`
  databases and exits 0.
- **`mariadb-install-db` is NOT in Herd's `bin/`** — it lives under
  `<basedir>/scripts/`. Resolver must derive `basedir` from the `mariadbd`
  binary (`<bin>/.. = basedir`) and locate the script there.
- **Real MySQL 8/9**: standard `mysqld --initialize-insecure --datadir=<data>`
  still applies. Branch on flavor via `--version` (string contains "MariaDB"
  ⇒ MariaDB; else MySQL).

## Resolver chain for Block D

```
1. ~/.config/hearth/services/mysql/bin/{mysqld,mariadbd}    (Hearth cache)
2. ~/Library/Application Support/Herd/bin/mysqld            (real MySQL if symlink valid)
3. ~/Library/Application Support/Herd/bin/mariadbd          (Herd MariaDB)
4. /opt/homebrew/opt/mysql/bin/mysqld
5. /opt/homebrew/opt/mariadb/bin/mariadbd
6. /usr/local/opt/mysql/bin/mysqld                          (Intel Mac)
7. /usr/local/opt/mariadb/bin/mariadbd                      (Intel Mac)
8. which mysqld / which mariadbd
```

Resolver returns `(binary, flavor: MysqlFlavor::{Mysql,MariaDB})`. Flavor used
only by the init wrapper script — runtime args are MySQL-compatible.

## Runtime args (flavor-agnostic)

```
<binary>
  --datadir=<data>
  --socket=<run>/mysql.sock
  --port=<port>
  --bind-address=127.0.0.1
  --pid-file=<run>/mysql.pid
  --log-error=<log>/mysql.err
  --skip-name-resolve
```

## Implications captured in plan deviations

1. Plan §3 said "Herd ships MariaDB-as-mysqld" — **inaccurate**. Herd's `mysqld`
   symlink is dangling on this machine; only `mariadbd` resolves. Resolver
   must probe both names.
2. Plan §4 init command `mysqld --initialize-insecure` — **wrong for MariaDB**.
   Branch needed.
3. Plan §9 LOC estimate for `db/mysql.rs` (130 LOC) still feasible, but the
   wrapper-script generator must invoke `mariadb-install-db` not `mysqld
   --initialize-insecure` when flavor is MariaDB.

Spike status: **DONE**. Block D path forward is unblocked.
