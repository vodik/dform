//! The Postgres provider, `dform-provider-postgres` (R-159): a server's
//! roles (`postgres.role`) and databases (`postgres.database`), read over
//! SQL (`pg_roles`, `pg_auth_members`, `pg_database`) and applied with
//! CREATE, ALTER and DROP, so a password the program rotates is an update
//! of the role, never a trigger. Written with the SDK (`dform-sdk`): the
//! schema is the types' derives, Plan the SDK's.
//!
//! `config` is how it is configured (`use postgres { url, password }`),
//! `conn` the connection (`tls` its TLS, `forward` a ClusterIP service
//! reached through the Kubernetes API), `sql` the quoting every statement
//! goes through, `scram` the verifier a password is sent as (never the
//! plaintext), `role` and `database` the types. `fake` (feature `fake`)
//! is a server for tests.

pub mod config;
pub mod conn;
pub mod database;
#[cfg(feature = "fake")]
pub mod fake;
pub mod forward;
pub mod role;
pub mod scram;
pub mod sql;
pub mod tls;

use dform_sdk::typed::{Error, Result};
use dform_sdk::{Provider, Typed};
use serde_json::{Value as Json, json};

/// The provider: a connection to one server, as its role (`user`).
pub struct Postgres {
    pub pool: conn::Pool,
}

impl Postgres {
    /// The role it connects as.
    pub fn user(&self) -> &str {
        &self.pool.settings().user
    }

    /// The open session; a failure to connect refuses the call, retryable.
    pub fn session(&self) -> Result<std::sync::Arc<conn::Session>> {
        Ok(self.pool.session()?)
    }
}

/// A statement's failure as a lifecycle function says it: the connection
/// lost in flight may have applied it.
pub fn failed(at: &str, f: conn::Failed) -> Error {
    match f {
        conn::Failed::Refused(m) => Error::Refused(format!("{at}: {m}")),
        conn::Failed::Lost(m) => Error::MaybeApplied(format!("{at}: {m}")),
    }
}

/// The comment dform leaves on what it creates: the idempotency key of
/// the create, so a create sent again answers what the first made.
pub fn mark(key: &str) -> String {
    format!("dform:{key}")
}

impl Provider for Postgres {
    const NAME: &'static str = "postgres";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    fn configure(settings: &Json) -> Result<(Postgres, Option<String>)> {
        let settings = config::Settings::parse(settings, &|k| std::env::var(k).ok())?;
        if let Some(w) = settings.warning() {
            dform_sdk::host().log.warn(&w);
        }
        let account = settings.account();
        Ok((
            Postgres {
                pool: conn::Pool::new(settings),
            },
            Some(account),
        ))
    }
}

/// The provider: its types, and the documents `dform provider check`
/// runs its resource checks with.
pub fn provider() -> Typed<Postgres> {
    Typed::new()
        .resource::<role::Role>()
        .resource::<database::Database>()
        .example::<role::Role>(
            json!({"name": "dform_check", "login": true, "connection_limit": 5}),
            json!({"name": "dform_check", "login": false, "connection_limit": 10}),
            "name",
        )
        .example::<role::Role>(
            json!({"name": "dform_check_pw", "login": true, "password": "hunter2"}),
            json!({"name": "dform_check_pw", "login": true, "password": "hunter3"}),
            "name",
        )
}

dform_sdk::provider!(provider(), uses = ["dform:host/log", "wasi:sockets/tcp"]);

#[cfg(test)]
mod tests {
    #[test]
    fn the_schema_is_the_derives() {
        let p = super::provider();
        let facts = p.facts();
        for line in [
            r#"type_attr("postgres.role", "name", "string", ["required", "force_new"])"#,
            r#"type_attr("postgres.role", "password", "string", ["sensitive", "write_only"])"#,
            r#"type_attr("postgres.role", "member_of", "set(ref(postgres.role))", [])"#,
            r#"type_attr("postgres.role", "login", "bool", ["optional_computed"])"#,
            r#"type_attr("postgres.database", "owner", "ref(postgres.role)", ["optional_computed"])"#,
            r#"type_attr("postgres.database", "encoding", "string", ["optional_computed", "force_new"])"#,
            r#"type_replace("postgres.database", "destroy_first")"#,
        ] {
            assert!(facts.contains(line), "{line}\n{facts}");
        }
    }
}
