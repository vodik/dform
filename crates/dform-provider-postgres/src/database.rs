//! `postgres.database`: a database, its owner, its encoding and locale.
//! Its remote id is its name. A new one is copied from `template0`, so
//! its encoding and locale are its own (the server's `template1` may
//! carry another's and objects besides); encoding and locale cannot
//! change after, so a change to them replaces it, as one to its name does.
//! The owner changes in place (`ALTER DATABASE .. OWNER TO`).

use crate::sql::{check_name, ident, literal};
use crate::{Postgres, failed, mark};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Lifecycle, Progress, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// A database.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[dform(type = "postgres.database", replace = "destroy_first")]
pub struct Database {
    #[dform(required, force_new)]
    pub name: String,
    /// Its owner; the provider's own role when none is written.
    #[dform(ty = "ref(postgres.role)", optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// As the server names it (`UTF8`, `LATIN1`, `SQL_ASCII`).
    #[dform(optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[dform(optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lc_collate: Option<String>,
    #[dform(optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lc_ctype: Option<String>,
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The server's encodings, as `pg_encoding_to_char` names them: one
/// written another way (`utf8`, `UTF-8`) would read back as a change, and
/// a change replaces the database.
const ENCODINGS: [&str; 41] = [
    "BIG5",
    "EUC_CN",
    "EUC_JP",
    "EUC_JIS_2004",
    "EUC_KR",
    "EUC_TW",
    "GB18030",
    "GBK",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "JOHAB",
    "KOI8R",
    "KOI8U",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "MULE_INTERNAL",
    "SJIS",
    "SHIFT_JIS_2004",
    "SQL_ASCII",
    "UHC",
    "UTF8",
    "WIN866",
    "WIN874",
    "WIN1250",
    "WIN1251",
    "WIN1252",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1256",
    "WIN1257",
];

fn at(remote: &str) -> String {
    format!("postgres.database {remote:?}")
}

const SELECT: &str = "SELECT d.datname AS name, pg_get_userbyid(d.datdba) AS owner, \
     pg_encoding_to_char(d.encoding) AS encoding, d.datcollate AS lc_collate, \
     d.datctype AS lc_ctype, shobj_description(d.oid, 'pg_database') AS comment \
     FROM pg_database d WHERE d.datname = ";

fn find(p: &Postgres, name: &str) -> Result<Option<(Database, Option<String>)>> {
    let rows = p
        .session()?
        .query(format!("{SELECT}{}", literal(name)))
        .map_err(|f| failed(&at(name), f))?;
    Ok(rows.into_iter().next().map(|row| {
        let text = |c: &str| row.get(c).cloned().flatten();
        (
            Database {
                name: text("name").unwrap_or_default(),
                owner: text("owner"),
                encoding: text("encoding"),
                lc_collate: text("lc_collate"),
                lc_ctype: text("lc_ctype"),
                id: text("name"),
            },
            text("comment"),
        )
    }))
}

impl Lifecycle<Postgres> for Database {
    fn read(p: &Postgres, remote: &str) -> Result<Option<Database>> {
        Ok(find(p, remote)?.map(|(d, _)| d))
    }

    fn create(
        p: &Postgres,
        desired: Database,
        key: &str,
        progress: &Progress,
    ) -> Result<(String, Database)> {
        let name = desired.name.clone();
        let at = at(&name);
        check_name("name", &name).map_err(|e| Error::Refused(format!("{at}: {e}")))?;
        if let Some((made, comment)) = find(p, &name)? {
            if comment.as_deref() == Some(mark(key).as_str()) {
                return Ok((name, made));
            }
            return Err(Error::Refused(format!(
                "{at}: a database of that name exists, not made by this resource: adopt it \
                 (`adopt(RESOURCE, {name:?})`) or name another"
            )));
        }
        let mut sql = format!("CREATE DATABASE {} WITH TEMPLATE template0", ident(&name));
        if let Some(o) = &desired.owner {
            sql.push_str(&format!(" OWNER {}", ident(o)));
        }
        if let Some(e) = &desired.encoding {
            sql.push_str(&format!(" ENCODING {}", literal(e)));
        }
        if let Some(c) = &desired.lc_collate {
            sql.push_str(&format!(" LC_COLLATE {}", literal(c)));
        }
        if let Some(c) = &desired.lc_ctype {
            sql.push_str(&format!(" LC_CTYPE {}", literal(c)));
        }
        progress.message(&format!("CREATE DATABASE {}", ident(&name)));
        let session = p.session()?;
        // Not in a transaction: CREATE DATABASE cannot be. The comment
        // follows it; a create sent again between the two finds a
        // database it did not mark, and says so.
        session.execute(sql).map_err(|f| failed(&at, f))?;
        session
            .execute(format!(
                "COMMENT ON DATABASE {} IS {}",
                ident(&name),
                literal(&mark(key))
            ))
            .map_err(|f| failed(&at, f))?;
        let made = find(p, &name)?.ok_or_else(|| {
            Error::MaybeApplied(format!("{at}: not there after its CREATE DATABASE"))
        })?;
        Ok((name, made.0))
    }

    fn update(
        p: &Postgres,
        remote: &str,
        prior: Database,
        desired: Database,
        _: &Progress,
    ) -> Result<Database> {
        let at = at(remote);
        if let Some(o) = &desired.owner
            && prior.owner.as_ref() != Some(o)
        {
            p.session()?
                .execute(format!(
                    "ALTER DATABASE {} OWNER TO {}",
                    ident(remote),
                    ident(o)
                ))
                .map_err(|f| failed(&at, f))?;
        }
        find(p, remote)?
            .map(|(d, _)| d)
            .ok_or_else(|| Error::Refused(format!("{at}: gone during its update")))
    }

    fn delete(p: &Postgres, remote: &str, _: &Progress) -> Result<()> {
        let at = at(remote);
        if remote == p.pool.settings().database {
            return Err(Error::Refused(format!(
                "{at}: it is the database provider postgres connects to (database = \
                 {remote:?}); connect to another (\"postgres\") to drop it"
            )));
        }
        p.session()?
            .execute(format!("DROP DATABASE IF EXISTS {}", ident(remote)))
            .map_err(|f| failed(&at, f))
    }

    fn check(_: &Postgres, desired: &Json) -> Result<()> {
        if let Some(e) = desired.get("encoding").and_then(Json::as_str)
            && !ENCODINGS.contains(&e)
        {
            let hint = ENCODINGS
                .iter()
                .find(|x| x.replace('_', "") == e.to_uppercase().replace(['-', '_'], ""))
                .map(|x| format!(": write {x:?}, as the server names it"))
                .unwrap_or_default();
            return Err(Error::Refused(format!(
                "encoding {e:?} is not an encoding as Postgres names it{hint}"
            )));
        }
        Ok(())
    }
}
