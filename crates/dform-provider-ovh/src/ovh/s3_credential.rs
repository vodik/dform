//! S3 credentials (`ovh.s3_credential`): the API's
//! `/cloud/project/{p}/user/{u}/s3Credentials/{access}`, one of a user's
//! access keys. Its remote id is `USER/ACCESS`. The API answers the
//! secret to the create, and after it only to a call of its own (`POST
//! …/secret`), never to a read: the secret is a sensitive computed value
//! the provider holds (R-45), answered as its label, and Reveal reads it
//! there with the deployment's lease. A credential is a type, not an
//! attribute of its user: a user may have several, each named and
//! revealed on its own, and one is rotated by making another.

use super::*;

/// The only secret this provider holds: a credential's secret.
const SECRET: &str = "secret";

impl Ovh {
    /// The credential, if its user still lists it.
    pub(super) fn read_s3_credential(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<(Json, Json)>> {
        let (user, access) = map::s3_credential_parts(remote);
        let Some(list) = a.client.get_opt(&credentials_path(p, user))? else {
            return Ok(None);
        };
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| s(c, "access") == Some(access))
            .map(|c| map::s3_credential(user, c)))
    }

    /// The credential `access` of `user`, if the user lists it.
    pub(super) fn find_s3_credential(&self, user: &str, access: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find an S3 credential")?;
        let remote = map::s3_credential_remote(user, access);
        Ok(self.read_s3_credential(&a, &p, &remote)?.map(|_| remote))
    }

    /// POST a credential for the program's user, with no body (the API
    /// refuses one, `cassettes/input-body.json`). The secret in the answer
    /// is dropped: Reveal reads it from the API. Nothing of the program's
    /// names a credential (the API makes its access key), so what this
    /// process made is kept under `key`, for a Create sent again.
    pub(super) fn create_s3_credential(
        &self,
        at: &str,
        config: &Json,
        key: &str,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let user = need(at, config, "user")?;
        storage::user_id(at, user)?;
        let o = a
            .client
            .post(&credentials_path(&p, user), None)
            .map_err(|e| failed(at, e))?;
        let access = s(&o, "access")
            .ok_or_else(|| Failed::MaybeApplied(format!("{at}: the API answered no access key")))?;
        if !key.is_empty() {
            let made = Made {
                typ: S3_CREDENTIAL.to_string(),
                key: vec![
                    ("user".to_string(), user.to_string()),
                    ("access".to_string(), access.to_string()),
                ],
            };
            self.made
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.to_string(), made);
        }
        let remote = map::s3_credential_remote(user, access);
        let (attrs, computed) = self.read_made(S3_CREDENTIAL, at, &remote, None)?;
        Ok((remote, attrs, computed))
    }

    /// A credential deleted; gone already (or its user gone) is done.
    pub(super) fn delete_s3_credential(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let (user, access) = map::s3_credential_parts(remote);
        let path = format!("{}/{}", credentials_path(&p, user), escape(access));
        self.delete_at(&a, at, &path, false, notes, say)
    }

    /// A credential's secret, for the engine to configure another
    /// provider with or to print (`output NAME --reveal`): only with the
    /// deployment's lease, read from the API by the call of its own.
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
        if h.r#type != S3_CREDENTIAL || h.path != SECRET {
            return Err(CallError::Refused(format!(
                "{at}: the ovh provider holds no secret there (it holds an {S3_CREDENTIAL}'s {SECRET})"
            )));
        }
        let (a, p) = self.project(&at).map_err(invalid)?;
        let (user, access) = map::s3_credential_parts(&h.remote);
        let secret = a
            .client
            .post(
                &format!("{}/{}/secret", credentials_path(&p, user), escape(access)),
                None,
            )
            .map_err(|e| CallError::Refused(format!("{at}: {e}")))?;
        let secret = s(&secret, "secret")
            .ok_or_else(|| CallError::Refused(format!("{at}: the API answered no secret")))?;
        Ok(pb::RevealResponse {
            value: secret.as_bytes().to_vec(),
        })
    }
}

/// The S3 credentials of the user `user` of the project `p`.
fn credentials_path(p: &str, user: &str) -> String {
    format!("/cloud/project/{p}/user/{}/s3Credentials", escape(user))
}
