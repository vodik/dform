# The Postgres provider

`dform-provider-postgres` manages a Postgres server's roles and
databases: it reads them over SQL (`pg_roles`, `pg_auth_members`,
`pg_database`) and applies a change with `CREATE`, `ALTER`, `GRANT`,
`REVOKE` and `DROP`. A password the program rotates (`dform secrets
rotate`) is then an update of the role, reviewed in a plan like any other
change, never a trigger or a job that runs `ALTER ROLE` on the side. It is
a native provider written with the SDK (`crates/dform-sdk`, the first real
one): dform starts it, and it dials the server itself.

## Configure

Build it with the workspace (`cargo build --workspace` puts it at
`target/debug/dform-provider-postgres`) and name it in `dform.toml`:

```toml
[providers]
postgres = { path = "~/src/dform/target/debug/dform-provider-postgres" }
```

The program says where the server is and who to connect as; the password
is a secret, revealed into the provider's Configure and kept in its
memory only (never in state, the plan file or the log):

```dform
use postgres {
  url = "postgres://dform_admin@db.example:5432/postgres"
  password = admin_password
}

use postgres {
  host = "db.example"        # or the url's
  port = 5432                # 5432 unless written
  database = "postgres"      # the one it connects to; "postgres" unless written
  user = "dform_admin"
  password = admin_password
  sslmode = "verify-full"    # "require" unless written
  root_cert = ca.certificate # PEM, for verify-ca and verify-full
}
```

A password in the url is refused (`postgres://u:pw@..`): written as
`password = ..` it is a secret and never printed. A program that names no
server at all (no `url`, no `host`) is configured from libpq's
environment, `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGPASSWORD` and
`PGSSLMODE`; `dform provider check` is run so. The provider reports
`USER@HOST:PORT/DATABASE` as its account, so `expect_account` holds a
deployment to one server.

### TLS

`sslmode` is libpq's, less the two that fall back to the clear without
saying:

| sslmode | the connection |
|---|---|
| `require` (the default) | TLS; the certificate is not checked (libpq's `require`) |
| `verify-ca` | TLS; the chain is checked against `root_cert`, else the machine's roots |
| `verify-full` | as `verify-ca`, and the certificate is for the host's name |
| `disable` | in the clear, only when written; Configure prints a warning |
| `prefer`, `allow` | refused: they fall back to the clear when the server has no TLS |

```
provider postgres: warning: sslmode=disable: the connection to dform_admin@db:5432/postgres is not encrypted: what the provider sends (each statement, the password verifiers) crosses the network in the clear
```

### The admin role

The provider connects as a role that no resource manages. A provider that
changed its own role (rotated its password, took its LOGIN away) or
dropped it would lock itself out in the middle of an apply, so a program
that manages the role in `user` is refused at plan, before anything
changes:

```
Error: plan postgres.role["admin"]: name: "dform_admin" is the role provider postgres connects as (user = "dform_admin"): changing it (a password rotated, LOGIN taken away) or dropping it would lock the provider out mid-apply. Connect as a separate admin role that no resource manages (`use postgres { user = .. }`; docs/providers/postgres.md, "The admin role")
```

The admin role is the one the server's bootstrap makes (the image's
`POSTGRES_USER`), with its own password; the application's role is a
resource. A component that runs Postgres says both:

```dform
let admin_password = random.password("${name}-admin-password")
let password = random.password("${name}-password")

resource k8s.secret creds {
  metadata = { name: "${name}-creds", namespace: namespace.metadata.name }
  # Read by the image once, when it makes the data directory.
  stringData = { POSTGRES_USER: "dform_admin", POSTGRES_PASSWORD: admin_password }
}
```

The provider also refuses to drop the database it connects to.

### A ClusterIP service: through the Kubernetes API

A database inside a cluster is a ClusterIP service (`synapse-db.apps.svc`)
that a laptop has no route to. With `kubeconfig` the provider reaches it
through the Kubernetes API, as `kubectl port-forward svc/NAME` does, in
its own process: it reads the service (its selector and the target port,
a name resolved in the pod), picks a ready pod it selects, and speaks
Postgres over the API server's port-forward of that pod (a WebSocket,
`v4.channel.k8s.io`). The host is the service's DNS name,
`SERVICE.NAMESPACE.svc` (`.cluster.local` or not), or `service` and
`namespace` say it:

```dform
use postgres {
  host = synapse_db.admin.host            # synapse-db.apps.svc
  user = synapse_db.admin.user
  password = synapse_db.admin.password
  kubeconfig = platform[env].kubeconfig   # a secret: revealed into Configure
  sslmode = "disable"                     # the stock image serves no TLS
}
```

The kubeconfig needs `get` on the service, `list` on pods and `create` on
`pods/portforward` in its namespace. Over the forward the API server's TLS
covers the hop to the node and the kubelet carries it to the pod, so
`sslmode` stays the operator's choice; `disable` still prints its warning,
saying so. dform running in the cluster (a controller) reaches the
service's name directly, without `kubeconfig`.

## Resources

| type | the server's object | remote id |
|---|---|---|
| `postgres.role` | a role (`pg_roles`) and the roles it is a member of (`pg_auth_members`) | its name |
| `postgres.database` | a database (`pg_database`) | its name |

`postgres.role`

| attribute | type | |
|---|---|---|
| `name` | string | required; a change replaces it |
| `password` | string, sensitive | write-only (below) |
| `login`, `superuser`, `createdb`, `createrole`, `inherit`, `replication` | bool | the server's default where not written (`NOLOGIN`, `INHERIT`, ..) |
| `connection_limit` | int | -1, no limit, unless written |
| `member_of` | set(ref(postgres.role)) | `GRANT g TO r`, `REVOKE` in place |
| `id` | string, computed | its name: what a reference to it (an owner, a membership) is |

`postgres.database`

| attribute | type | |
|---|---|---|
| `name` | string | required; a change replaces it |
| `owner` | ref(postgres.role) | `ALTER DATABASE .. OWNER TO` in place; the provider's role unless written |
| `encoding` | string | as the server names it (`UTF8`, `LATIN1`; `utf-8` is refused at plan, since it would read back as a change); a change replaces it |
| `lc_collate`, `lc_ctype` | string | a change replaces it |
| `id` | string, computed | its name |

The flags, the limit, the owner, the encoding and the locale are
Optional+Computed: dform compares one only where the program writes it,
so a role adopted with only its name is not changed. A new database is
copied from `template0`, so its encoding and locale are its own (Synapse
needs `lc_collate = "C"`); `template1` may carry another's and objects
besides. A replace of either type deletes first (two objects of one name
cannot exist at once): replacing a database drops it and its data, so
hold one with `lifecycle(r, "prevent_destroy")`.

A create leaves its idempotency key as the object's comment
(`COMMENT ON ROLE .. IS 'dform:KEY'`): sent again after a lost answer it
answers what it made, and a role or database of that name made elsewhere
is refused, saying to adopt it (`adopt(r, "synapse")`).

### Passwords

The server keeps a verifier of a password, never the password, so Read
never answers one: `password` is write-only. dform keeps its
keyed digest in state and compares the program's value with it; a
different one is a change, an update of the role. The provider sends a
SCRAM-SHA-256 verifier it computes (`SCRAM-SHA-256$4096:SALT$STOREDKEY:SERVERKEY`,
a fresh salt each time), never the plaintext, so the password reaches
neither the server's statement log nor `pg_stat_activity` nor an error it
echoes. An update that carries the password (a role's LOGIN changed) sends
it only when the verifier the server keeps is not of it, read from
`pg_authid`, which only a superuser may; a provider connected as a
`CREATEROLE` role that is not one sends it with each such update.

A rotation is one command and one plan line:

```text
$ dform secrets rotate apps env=lab synapse-db-password
$ dform plan apps env=lab
  ~ postgres.role synapse_role  stacks/apps.df:40
      password = (sensitive) → (sensitive, generation 2, rotated 2026-10-08 by simon@host)
  ~ k8s.secret synapse_config   synapse.df:20
      ..
```

The role's update and every reader of the same key (the application's
configuration) are in the same plan. A run without the master
cannot send the new password: dform stops the role's update and says the
master is needed.

A password another provider holds (a secret output read where it is
held) is refused: the provider takes one the program derives or is
given.

## Not yet

- Privileges: a database's `GRANT CONNECT`/`CREATE`/`TEMPORARY` and the
  grants inside a database (schemas, tables, default privileges) are not
  managed. An owner covers an application that owns its database; the
  in-database grants need a connection to each database, a type of their
  own (`postgres.grant`), not a set attribute of the role.
- Schemas and extensions (`CREATE EXTENSION`).
- A password removed from the program is left on the role as it is
  (`PASSWORD NULL` is not sent).
- The `managed` capability (`provider.created`): the comment is the key,
  but Query is not served by the SDK's typed layer.
- A component build: the provider dials the server itself (tokio-postgres
  over tokio), so it is native only; its manifest declares
  `wasi:sockets/tcp`.
- The port-forward is the provider's own: a host transport
  (`k8s-portforward://NAMESPACE/SERVICE:PORT` through the k8s provider's
  credentials) would let any provider dial a ClusterIP service.

## Tests

`dform_provider_postgres::fake` (feature `fake`) is a Postgres server in
the test process: the wire protocol's startup, TLS, SCRAM-SHA-256 against
each role's verifier (so a verifier the provider computed is proven by
logging in with the password) and the simple query protocol, answering the
provider's statements over a catalog in memory and keeping every query
as it arrived. `fake::kube` is a Kubernetes API whose port-forward carries
bytes to it. `crates/dform-provider-postgres/tests/fake.rs` calls the
provider as dform does; `tests/provider_postgres.rs` runs dform with it.
