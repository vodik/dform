//! Users of the project (`ovh.cloud_project_user`): the API's
//! `/cloud/project/{p}/user/{id}`, an OpenStack user with roles, named
//! by its description (the API makes up its username). The API makes its
//! password and answers it once, when the user is made: dform never has
//! it. Its S3 credentials are objects of their own (`s3_credential`).

use super::*;

impl Ovh {
    /// The user, unless it is deleted.
    pub(super) fn read_user(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<(Json, Json)>> {
        let path = format!("/cloud/project/{p}/user/{}", escape(remote));
        let Some(o) = a.client.get_opt(&path)? else {
            return Ok(None);
        };
        if s(&o, "status") == Some("deleted") {
            return Ok(None);
        }
        Ok(Some(map::user(&o)))
    }

    /// POST the user and wait for it to be `ok` (`creating` until then).
    /// The password the answer carries is dropped.
    pub(super) fn create_user(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let body = json!({
            "description": need(at, config, "description")?,
            "roles": roles(at, config)?,
        });
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/user"), Some(&body))
            .map_err(|e| failed(at, e))?;
        let id = o.get("id").and_then(Json::as_i64).unwrap_or(0).to_string();
        let path = format!("/cloud/project/{p}/user/{id}");
        let mut o = o;
        if let Some(m) = o.as_object_mut() {
            m.remove("password");
        }
        self.settle(
            &a,
            at,
            &path,
            o,
            &["ok"],
            &["disabled"],
            CREATE_WAIT,
            notes,
            say,
        )?;
        let (attrs, computed) = self.read_made(USER, at, &id, None)?;
        Ok((id, attrs, computed))
    }

    /// Its roles, set at once to the program's (by the ids the project
    /// gives the roles' names), `now` its attributes as read.
    pub(super) fn update_user(
        &self,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = format!("/cloud/project/{p}/user/{}", escape(remote));
        let mut want = roles(at, config)?;
        want.sort();
        if now.get("roles") == Some(&json!(want)) {
            return Ok(());
        }
        let all = a
            .client
            .get(&format!("/cloud/project/{p}/role"))
            .map_err(|e| failed(at, e))?;
        let ids = want
            .iter()
            .map(|name| {
                all.get("roles")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .find(|r| s(r, "name") == Some(name))
                    .and_then(|r| s(r, "id"))
                    .ok_or_else(|| refused(at, format!("the project has no role {name}")))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        a.client
            .put(&format!("{path}/role"), &json!({"rolesIds": ids}))
            .map_err(|e| failed(at, e))?;
        Ok(())
    }

    /// The user of `description`, unless it is deleted.
    pub(super) fn find_user(&self, description: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find a user")?;
        let list = a.client.get(&format!("/cloud/project/{p}/user"))?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| !matches!(s(o, "status"), Some("deleted" | "deleting")))
            .find(|o| s(o, "description") == Some(description))
            .and_then(|o| o.get("id").and_then(Json::as_i64))
            .map(|id| id.to_string()))
    }

    pub(super) fn delete_user(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = format!("/cloud/project/{p}/user/{}", escape(remote));
        self.delete_at(&a, at, &path, true, notes, say)
    }
}

/// The program's roles, names as the API spells them.
fn roles(at: &str, config: &Json) -> std::result::Result<Vec<String>, Failed> {
    match config.get("roles") {
        Some(Json::Array(rs)) => rs
            .iter()
            .map(|r| match r {
                Json::String(r) => Ok(r.clone()),
                v => Err(refused(
                    at,
                    format!(
                        "roles: {} is not a role's name",
                        provider::fmt_value(Some(v))
                    ),
                )),
            })
            .collect(),
        Some(v) if marker(v).is_some() => Err(refused(
            at,
            format!(
                "roles is {}, not a value the provider can send",
                provider::fmt_value(Some(v))
            ),
        )),
        _ => Err(refused(at, "roles is not set")),
    }
}
