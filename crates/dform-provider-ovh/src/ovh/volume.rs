//! Block volumes (`ovh.volume`): the API's `/cloud/project/{p}/volume/{id}`,
//! named by their name in a region. The instance a volume is attached to
//! is its attribute, `instance = server`: the API keeps the attachment on
//! the volume (`attachedTo`, `POST /attach`, `POST /detach`), so there is
//! no attachment object, and the reference orders the volume after the
//! instance. Setting it attaches the volume once it is `available`,
//! clearing it detaches it, changing it detaches and attaches. One
//! instance at a time: multi-attach volumes are not served. A volume
//! grows in place (`POST /upsize`); a smaller size, another region, type,
//! image or snapshot replaces it. Each wait says the volume's status as
//! the API gives it (`creating`, `available`, `attaching`, `in-use`).

use super::*;

/// How long an attachment or a detachment waits for the volume to settle.
const ATTACH_WAIT: Duration = Duration::from_secs(5 * 60);

/// The statuses a volume stops in when a change fails.
const FAILED: [&str; 3] = ["error", "error_extending", "error_deleting"];

impl Ovh {
    /// The volume as the API answers it, unless it is gone.
    pub(super) fn read_volume(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<Json>> {
        Ok(a.client
            .get_opt(&volume_path(p, remote))?
            .filter(|o| s(o, "status") != Some("deleted")))
    }

    pub(super) fn create_volume(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let region = need(at, config, "region")?;
        let mut body = json!({
            "name": need(at, config, "name")?,
            "region": region,
            "size": size(at, config)?,
        });
        for (k, api) in [("type", "type"), ("description", "description")] {
            if config.get(k).is_some() {
                body[api] = json!(need(at, config, k)?);
            }
        }
        if config.get("image").is_some() {
            let image = need(at, config, "image")?;
            body["imageId"] = json!(
                self.image_id(&a, &p, region, image)
                    .map_err(|e| refused(at, format!("image: {e:#}")))?
            );
        }
        if config.get("snapshot").is_some() {
            body["snapshotId"] = json!(need(at, config, "snapshot")?);
        }
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/volume"), &body)
            .map_err(|e| failed(at, e))?;
        let id = s(&o, "id").unwrap_or_default().to_string();
        let path = volume_path(&p, &id);
        let o = self.settle(
            &a,
            at,
            &path,
            o,
            &["available"],
            &FAILED,
            CREATE_WAIT,
            notes,
            say,
        )?;
        let o = match instance(at, config)? {
            Some(i) if s(&o, "status") == Some("available") => {
                self.attach(&a, at, &path, i, notes, say)?
            }
            _ => o,
        };
        let (attrs, computed) = map::volume(&o);
        Ok((id, attrs, computed))
    }

    /// Its name and description in place, its size grown, and its
    /// instance: detached from the one it is on, attached to the
    /// program's.
    pub(super) fn update_volume(
        &self,
        at: &str,
        remote: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = volume_path(&p, remote);
        let now = self
            .read_volume(&a, &p, remote)
            .map_err(|e| refused(at, e))?
            .ok_or_else(|| refused(at, format!("volume {remote} is not there")))?;
        let mut put = json!({});
        for k in ["name", "description"] {
            if let Some(v) = config.get(k)
                && now.get(k) != Some(v)
            {
                put[k] = json!(need(at, config, k)?);
            }
        }
        if put.as_object().is_some_and(|m| !m.is_empty()) {
            a.client.put(&path, &put).map_err(|e| failed(at, e))?;
        }
        let want = size(at, config)?;
        let was = now.get("size").and_then(Json::as_i64).unwrap_or(0);
        if want > was {
            let o = a
                .client
                .post(&format!("{path}/upsize"), &json!({"size": want}))
                .map_err(|e| failed(at, e))?;
            self.settle(
                &a,
                at,
                &path,
                o,
                &["available", "in-use"],
                &FAILED,
                ATTACH_WAIT,
                notes,
                say,
            )?;
        }
        let want = instance(at, config)?;
        let on = map::attached_to(&now);
        if on.iter().map(String::as_str).eq(want) {
            return Ok(());
        }
        for i in &on {
            self.detach(&a, at, &path, i, notes, say)?;
        }
        if let Some(i) = want {
            self.attach(&a, at, &path, i, notes, say)?;
        }
        Ok(())
    }

    /// Detached from every instance it is on first: the API refuses to
    /// delete a volume in use.
    pub(super) fn delete_volume(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = volume_path(&p, remote);
        if let Some(now) = self
            .read_volume(&a, &p, remote)
            .map_err(|e| refused(at, e))?
        {
            for i in map::attached_to(&now) {
                self.detach(&a, at, &path, &i, notes, say)?;
            }
        }
        self.delete_at(&a, at, &path, true, notes, say)
    }

    fn attach(
        &self,
        a: &Account,
        at: &str,
        path: &str,
        instance: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<Json, Failed> {
        let o = a
            .client
            .post(&format!("{path}/attach"), &json!({"instanceId": instance}))
            .map_err(|e| failed(&format!("{at}: attach to instance {instance}"), e))?;
        self.settle(
            a,
            at,
            path,
            o,
            &["in-use"],
            &FAILED,
            ATTACH_WAIT,
            notes,
            say,
        )
    }

    fn detach(
        &self,
        a: &Account,
        at: &str,
        path: &str,
        instance: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<Json, Failed> {
        let o = a
            .client
            .post(&format!("{path}/detach"), &json!({"instanceId": instance}))
            .map_err(|e| failed(&format!("{at}: detach from instance {instance}"), e))?;
        self.settle(
            a,
            at,
            path,
            o,
            &["available"],
            &FAILED,
            ATTACH_WAIT,
            notes,
            say,
        )
    }
}

fn volume_path(p: &str, remote: &str) -> String {
    format!("/cloud/project/{p}/volume/{}", escape(remote))
}

/// Its size in whole GiB, as the schema's `bytes(gib)` sends it.
fn size(at: &str, config: &Json) -> std::result::Result<i64, Failed> {
    match config.get("size") {
        Some(Json::Number(n)) => n
            .as_i64()
            .ok_or_else(|| refused(at, format!("size {n} is not a whole number of GiB"))),
        Some(v) if marker(v).is_some() => Err(refused(
            at,
            format!(
                "size is {}, not a value the provider can send",
                provider::fmt_value(Some(v))
            ),
        )),
        _ => Err(refused(at, "size is not set")),
    }
}

/// The instance the program attaches it to: the instance's id.
fn instance<'a>(at: &str, config: &'a Json) -> std::result::Result<Option<&'a str>, Failed> {
    match config.get("instance") {
        None | Some(Json::Null) => Ok(None),
        Some(_) => need(at, config, "instance").map(Some),
    }
}
