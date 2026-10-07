//! `postgres.role`: a role, its attributes, its password and the roles it
//! is a member of. Its remote id is its name (what `ALTER ROLE` takes,
//! and what a reference to it, an owner or a membership, resolves to).
//!
//! The password is write-only (R-106): the server keeps a verifier, never
//! the password, so Read never answers it and dform keeps its digest in
//! state; a changed one is an update. It is sent as a SCRAM-SHA-256
//! verifier the provider computes (`scram`), never as the plaintext, and
//! only when the verifier the server keeps is not of it already (read from
//! `pg_authid` when the provider's role may; else it is sent).

use crate::conn::Row;
use crate::sql::{check_name, ident, literal};
use crate::{Postgres, failed, mark, scram};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Lifecycle, Progress, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeSet;

/// A role.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[dform(type = "postgres.role", replace = "destroy_first")]
pub struct Role {
    #[dform(required, force_new)]
    pub name: String,
    /// The program's password: a string, or a secret's marker when it is
    /// held elsewhere (refused: the provider reads none).
    #[dform(ty = "string", sensitive, write_only)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<Json>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<bool>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superuser: Option<bool>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub createdb: Option<bool>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub createrole: Option<bool>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit: Option<bool>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication: Option<bool>,
    /// Concurrent connections it may have; -1 for no limit.
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_limit: Option<i64>,
    /// The roles it is a member of (`GRANT g TO r`).
    #[dform(ty = "set(ref(postgres.role))")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_of: Vec<String>,
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The attribute flags, each as `CREATE ROLE` and `ALTER ROLE` spell it.
const FLAGS: [(&str, &str); 6] = [
    ("login", "LOGIN"),
    ("superuser", "SUPERUSER"),
    ("createdb", "CREATEDB"),
    ("createrole", "CREATEROLE"),
    ("inherit", "INHERIT"),
    ("replication", "REPLICATION"),
];

impl Role {
    fn flag(&self, name: &str) -> Option<bool> {
        match name {
            "login" => self.login,
            "superuser" => self.superuser,
            "createdb" => self.createdb,
            "createrole" => self.createrole,
            "inherit" => self.inherit,
            "replication" => self.replication,
            _ => None,
        }
    }

    /// The options of `self` that `prior` does not have as they are
    /// (every one it sets, with no prior): `LOGIN NOSUPERUSER ..`.
    fn options(&self, prior: Option<&Role>) -> Vec<String> {
        let mut out = Vec::new();
        for (name, kw) in FLAGS {
            if let Some(v) = self.flag(name)
                && prior.and_then(|p| p.flag(name)) != Some(v)
            {
                out.push(match v {
                    true => kw.to_string(),
                    false => format!("NO{kw}"),
                });
            }
        }
        if let Some(n) = self.connection_limit
            && prior.and_then(|p| p.connection_limit) != Some(n)
        {
            out.push(format!("CONNECTION LIMIT {n}"));
        }
        out
    }

    /// The password the program gives, as text; none when it gives none.
    fn password(&self, at: &str) -> Result<Option<&str>> {
        match &self.password {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(s)) => Ok(Some(s)),
            Some(v) => Err(Error::Refused(format!(
                "{at}: password is a secret {}: the postgres provider takes a password \
                 the program derives or is given (random.password, an input), not one \
                 another provider holds",
                held_by(v)
            ))),
        }
    }
}

/// Who holds a secret marker, for a refusal.
fn held_by(v: &Json) -> String {
    match v
        .get("held")
        .and_then(|h| h.get("provider"))
        .and_then(Json::as_str)
    {
        Some(p) => format!("provider {p} holds"),
        None => "no provider holds yet".to_string(),
    }
}

fn at(remote: &str) -> String {
    format!("postgres.role {remote:?}")
}

fn bool_of(row: &Row, col: &str) -> Option<bool> {
    row.get(col)?.as_deref().map(|v| v == "t" || v == "true")
}

const SELECT: &str = "SELECT r.rolname AS name, r.rolsuper AS superuser, \
     r.rolinherit AS inherit, r.rolcreaterole AS createrole, r.rolcreatedb AS createdb, \
     r.rolcanlogin AS login, r.rolreplication AS replication, \
     r.rolconnlimit AS connection_limit, shobj_description(r.oid, 'pg_authid') AS comment, \
     (SELECT coalesce(json_agg(DISTINCT g.rolname ORDER BY g.rolname), '[]') \
     FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid WHERE m.member = r.oid) \
     AS member_of FROM pg_roles r WHERE r.rolname = ";

/// The role `name` and dform's comment on it, if there is one.
fn find(p: &Postgres, name: &str) -> Result<Option<(Role, Option<String>)>> {
    let rows = p
        .session()?
        .query(format!("{SELECT}{}", literal(name)))
        .map_err(|f| failed(&at(name), f))?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let text = |c: &str| row.get(c).cloned().flatten();
    let member_of: Vec<String> =
        serde_json::from_str(&text("member_of").unwrap_or_else(|| "[]".into())).map_err(|e| {
            Error::Refused(format!(
                "{}: member_of as the server answers it: {e}",
                at(name)
            ))
        })?;
    let role = Role {
        name: text("name").unwrap_or_default(),
        password: None,
        login: bool_of(&row, "login"),
        superuser: bool_of(&row, "superuser"),
        createdb: bool_of(&row, "createdb"),
        createrole: bool_of(&row, "createrole"),
        inherit: bool_of(&row, "inherit"),
        replication: bool_of(&row, "replication"),
        connection_limit: text("connection_limit").and_then(|n| n.parse().ok()),
        member_of,
        id: text("name"),
    };
    Ok(Some((role, text("comment"))))
}

/// The verifier the server keeps for `name`, when the provider's role may
/// read it (`pg_authid` is a superuser's); `None` when it may not.
fn verifier(p: &Postgres, name: &str) -> Result<Option<String>> {
    let sql = format!(
        "SELECT rolpassword FROM pg_authid WHERE rolname = {}",
        literal(name)
    );
    match p.session()?.query(sql) {
        Ok(rows) => Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.get("rolpassword").cloned().flatten())),
        Err(crate::conn::Failed::Refused(_)) => Ok(None),
        Err(f) => Err(failed(&at(name), f)),
    }
}

fn names(xs: &[String]) -> String {
    xs.iter().map(|x| ident(x)).collect::<Vec<_>>().join(", ")
}

impl Lifecycle<Postgres> for Role {
    fn read(p: &Postgres, remote: &str) -> Result<Option<Role>> {
        Ok(find(p, remote)?.map(|(r, _)| r))
    }

    fn create(
        p: &Postgres,
        desired: Role,
        key: &str,
        progress: &Progress,
    ) -> Result<(String, Role)> {
        let name = desired.name.clone();
        let at = at(&name);
        check_name("name", &name).map_err(|e| Error::Refused(format!("{at}: {e}")))?;
        let password = desired.password(&at)?;
        if let Some((made, comment)) = find(p, &name)? {
            // Sent again: what the first create made.
            if comment.as_deref() == Some(mark(key).as_str()) {
                return Ok((name, made));
            }
            return Err(Error::Refused(format!(
                "{at}: a role of that name exists, not made by this resource: adopt it \
                 (`adopt(RESOURCE, {name:?})`) or name another"
            )));
        }
        let mut sql = format!("CREATE ROLE {}", ident(&name));
        let options = desired.options(None);
        if !options.is_empty() {
            sql.push_str(" WITH ");
            sql.push_str(&options.join(" "));
        }
        if let Some(pw) = password {
            sql.push_str(&format!(" PASSWORD {}", literal(&scram::verifier(pw))));
        }
        if !desired.member_of.is_empty() {
            sql.push_str(&format!(" IN ROLE {}", names(&desired.member_of)));
        }
        sql.push_str(&format!(
            ";\nCOMMENT ON ROLE {} IS {}",
            ident(&name),
            literal(&mark(key))
        ));
        progress.message(&format!("CREATE ROLE {}", ident(&name)));
        p.session()?.execute(sql).map_err(|f| failed(&at, f))?;
        let made = find(p, &name)?
            .ok_or_else(|| Error::MaybeApplied(format!("{at}: not there after its CREATE ROLE")))?;
        Ok((name, made.0))
    }

    fn update(
        p: &Postgres,
        remote: &str,
        prior: Role,
        desired: Role,
        progress: &Progress,
    ) -> Result<Role> {
        let at = at(remote);
        let role = ident(remote);
        let mut statements = Vec::new();
        let options = desired.options(Some(&prior));
        if !options.is_empty() {
            statements.push(format!("ALTER ROLE {role} WITH {}", options.join(" ")));
        }
        if let Some(pw) = desired.password(&at)? {
            let kept = verifier(p, remote)?;
            if !kept.is_some_and(|v| scram::matches(&v, pw)) {
                progress.message("a new password verifier");
                statements.push(format!(
                    "ALTER ROLE {role} PASSWORD {}",
                    literal(&scram::verifier(pw))
                ));
            }
        }
        let had: BTreeSet<&String> = prior.member_of.iter().collect();
        let want: BTreeSet<&String> = desired.member_of.iter().collect();
        for g in want.difference(&had) {
            statements.push(format!("GRANT {} TO {role}", ident(g)));
        }
        for g in had.difference(&want) {
            statements.push(format!("REVOKE {} FROM {role}", ident(g)));
        }
        if !statements.is_empty() {
            p.session()?
                .execute(statements.join(";\n"))
                .map_err(|f| failed(&at, f))?;
        }
        find(p, remote)?
            .map(|(r, _)| r)
            .ok_or_else(|| Error::Refused(format!("{at}: gone during its update")))
    }

    fn delete(p: &Postgres, remote: &str, _: &Progress) -> Result<()> {
        let at = at(remote);
        if remote == p.user() {
            return Err(Error::Refused(own_role(&at, p.user())));
        }
        p.session()?
            .execute(format!("DROP ROLE IF EXISTS {}", ident(remote)))
            .map_err(|f| failed(&at, f))
    }

    /// The role the provider connects as is not one it manages: an ALTER
    /// of it (its password rotated, LOGIN taken away) or its DROP locks
    /// the provider out mid-apply.
    fn check(p: &Postgres, desired: &Json) -> Result<()> {
        match desired.get("name").and_then(Json::as_str) {
            Some(name) if name == p.user() => Err(Error::Refused(own_role("name", name))),
            _ => Ok(()),
        }
    }
}

fn own_role(at: &str, user: &str) -> String {
    format!(
        "{at}: {user:?} is the role provider postgres connects as (user = {user:?}): \
         changing it (a password rotated, LOGIN taken away) or dropping it would lock the \
         provider out mid-apply. Connect as a separate admin role that no resource \
         manages (`use postgres {{ user = .. }}`; docs/providers/postgres.md, \
         \"The admin role\")"
    )
}
