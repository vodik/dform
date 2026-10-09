//! S3 containers (`ovh.storage_container`): the API's object storage
//! containers of a region, `/cloud/project/{p}/region/{r}/storage/{name}`,
//! S3-class only (the Swift containers of `/cloud/project/{p}/storage` are
//! not served). A container is named by its region and name, its remote
//! id `REGION/NAME`; only its versioning changes in place.

use super::*;

impl Ovh {
    pub(super) fn read_container(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<(Json, Json)>> {
        Ok(a.client
            .get_opt(&map::container_path(p, remote))?
            .map(|o| map::container(&o)))
    }

    pub(super) fn create_container(
        &self,
        at: &str,
        config: &Json,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let region = need(at, config, "region")?;
        let name = need(at, config, "name")?;
        let mut body = json!({"name": name});
        if config.get("owner").is_some() {
            body["ownerId"] = json!(user_id(at, need(at, config, "owner")?)?);
        }
        if config.get("versioning").and_then(Json::as_bool) == Some(true) {
            body["versioning"] = json!({"status": "enabled"});
        }
        let o = a
            .client
            .post(
                &format!("/cloud/project/{p}/region/{}/storage", escape(region)),
                &body,
            )
            .map_err(|e| failed(at, e))?;
        let (attrs, computed) = map::container(&o);
        Ok((map::container_remote(region, name), attrs, computed))
    }

    /// Versioning on is `enabled`; off again is `suspended`, as S3 has it
    /// (a container that had it never goes back to `disabled`). `now` is
    /// what Read computed.
    pub(super) fn update_container(
        &self,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
    ) -> std::result::Result<(), Failed> {
        let Some(want) = config.get("versioning").and_then(Json::as_bool) else {
            return Ok(());
        };
        if now.get("versioning").and_then(Json::as_bool) == Some(want) {
            return Ok(());
        }
        let (a, p) = self.project_for(at)?;
        let status = if want { "enabled" } else { "suspended" };
        a.client
            .put(
                &map::container_path(&p, remote),
                &json!({"versioning": {"status": status}}),
            )
            .map_err(|e| failed(at, e))?;
        Ok(())
    }

    /// The container named `name` in `region`.
    pub(super) fn find_container(&self, region: &str, name: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find an S3 container")?;
        Ok(self
            .read_container(&a, &p, &map::container_remote(region, name))?
            .map(|_| map::container_remote(region, name)))
    }

    pub(super) fn delete_container(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = map::container_path(&p, remote);
        self.delete_at(&a, at, &path, false, notes, say)
    }
}

/// A user's id as the API takes it, a number: what a reference to an
/// `ovh.cloud_project_user` is.
pub(super) fn user_id(at: &str, remote: &str) -> std::result::Result<i64, Failed> {
    remote
        .parse()
        .map_err(|_| refused(at, format!("{remote:?} is not a user's id")))
}
