//! SSH keys (`ovh.ssh_key`): the API's `/cloud/project/{p}/sshkey/{id}`, named
//! by their name. Nothing of a key changes in place: the schema replaces it.

use super::*;

impl Ovh {
    pub(super) fn read_ssh_key(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<(Json, Json)>> {
        Ok(a.client
            .get_opt(&format!("/cloud/project/{p}/sshkey/{}", escape(remote)))?
            .map(|o| map::ssh_key(&o)))
    }

    /// The key named `name`.
    pub(super) fn find_ssh_key(&self, name: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find an SSH key")?;
        let list = a.client.get(&format!("/cloud/project/{p}/sshkey"))?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| s(o, "name") == Some(name))
            .and_then(|o| s(o, "id"))
            .map(str::to_string))
    }

    pub(super) fn create_ssh_key(
        &self,
        at: &str,
        config: &Json,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let body = json!({
            "name": need(at, config, "name")?,
            "publicKey": need(at, config, "public_key")?,
        });
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/sshkey"), &body)
            .map_err(|e| failed(at, e))?;
        let (attrs, computed) = map::ssh_key(&o);
        Ok((s(&o, "id").unwrap_or_default().to_string(), attrs, computed))
    }

    /// A key deleted; gone already is done.
    pub(super) fn delete_ssh_key(&self, at: &str, remote: &str) -> std::result::Result<(), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        match a
            .client
            .delete(&format!("/cloud/project/{p}/sshkey/{}", escape(remote)))
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(failed(at, e)),
        }
    }
}
