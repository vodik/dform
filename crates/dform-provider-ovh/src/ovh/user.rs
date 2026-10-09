//! Users of the project (`ovh.cloud_project_user`): the API's
//! `/cloud/project/{p}/user/{id}`, an OpenStack user with roles, named
//! by its description (the API makes up its username). The API makes its
//! password and answers it once, when the user is made: dform never has
//! it. Each user has an S3 credential, made with it: the access key is a
//! computed attribute, the secret a sensitive one, which the API keeps
//! and the provider reads there only for Reveal (R-45, R-130), so a
//! provider configured with it (`secret_key = backup.s3_secret_key`) gets
//! the bytes, and dform only the label.

use super::*;

/// The only secret this provider holds: a user's S3 secret key.
const S3_SECRET: &str = "s3_secret_key";

impl Ovh {
    /// The user, and the access key of its first S3 credential.
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
        let access = first_access(&a.client.get(&format!("{path}/s3Credentials"))?);
        Ok(Some(map::user(&o, access.as_deref())))
    }

    /// POST the user, wait for it to be `ok` (`creating` until then), and
    /// make its S3 credential. The password the answer carries is dropped.
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
            .post(&format!("/cloud/project/{p}/user"), &body)
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
        self.s3_credential(&a, at, &path)?;
        let (attrs, computed) = self.read_made(USER, at, &id)?;
        Ok((id, attrs, computed))
    }

    /// Its roles, set at once to the program's (by the ids the project
    /// gives the roles' names); an S3 credential made if it has none.
    pub(super) fn update_user(
        &self,
        at: &str,
        remote: &str,
        now: &(Json, Json),
        config: &Json,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = format!("/cloud/project/{p}/user/{}", escape(remote));
        let mut want = roles(at, config)?;
        want.sort();
        if now.0.get("roles") != Some(&json!(want)) {
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
        }
        if now.1.get("s3_access_key").is_none_or(Json::is_null) {
            self.s3_credential(&a, at, &path)?;
        }
        Ok(())
    }

    /// Make an S3 credential for the user at `path`. Its secret in the
    /// answer is dropped: Reveal reads it from the API.
    fn s3_credential(&self, a: &Account, at: &str, path: &str) -> std::result::Result<(), Failed> {
        a.client
            .post(&format!("{path}/s3Credentials"), &json!({}))
            .map_err(|e| failed(at, e))?;
        Ok(())
    }

    /// A user's S3 secret, for the engine to configure another provider
    /// with (R-45): only with the deployment's lease, and only the secret
    /// of the credential Read answers.
    pub(super) fn reveal(
        &self,
        r: pb::RevealRequest,
    ) -> std::result::Result<pb::RevealResponse, CallError> {
        let h = r.held.unwrap_or_default();
        let at = format!("reveal {} {}#{}", h.r#type, h.remote, h.path);
        if r.lease.is_empty() {
            return Err(CallError::Refused(format!(
                "{at}: refused without the deployment's lease (a reveal is the engine's call)"
            )));
        }
        if h.r#type != USER || h.path != S3_SECRET {
            return Err(CallError::Refused(format!(
                "{at}: the ovh provider holds no secret there (it holds a user's {S3_SECRET})"
            )));
        }
        let (a, p) = self.project(&at).map_err(invalid)?;
        let path = format!("/cloud/project/{p}/user/{}", escape(&h.remote));
        let creds = a
            .client
            .get(&format!("{path}/s3Credentials"))
            .map_err(|e| CallError::Refused(format!("{at}: {e}")))?;
        let access = first_access(&creds)
            .ok_or_else(|| CallError::Refused(format!("{at}: the user has no S3 credential")))?;
        let secret = a
            .client
            .post(
                &format!("{path}/s3Credentials/{}/secret", escape(&access)),
                &json!({}),
            )
            .map_err(|e| CallError::Refused(format!("{at}: {e}")))?;
        let secret = s(&secret, "secret")
            .ok_or_else(|| CallError::Refused(format!("{at}: the API answered no secret")))?;
        Ok(pb::RevealResponse {
            value: secret.as_bytes().to_vec(),
        })
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

/// The access key of the first S3 credential in `list`
/// (`cloud.user.S3Credentials[]`).
fn first_access(list: &Json) -> Option<String> {
    list.as_array()?
        .first()
        .and_then(|c| s(c, "access"))
        .map(str::to_string)
}
