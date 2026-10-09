//! Instances (`ovh.instance`): the API's `/cloud/project/{p}/instance/{id}`,
//! named by their name in a region. A Create waits for the instance to be
//! ACTIVE, saying each status the API gives (`BUILD`, `ACTIVE`); its flavor
//! and image are checked against what the region offers (`catalog`). A
//! larger flavor resizes it in place (it reboots), a smaller one replaces it;
//! its networks are interfaces (`network`). A Delete waits for it to go.

use super::*;

impl Ovh {
    pub(super) fn read_instance(
        &self,
        a: &Account,
        p: &str,
        id: &str,
    ) -> api::Result<Option<(Json, Json)>> {
        let Some(o) = a
            .client
            .get_opt(&format!("/cloud/project/{p}/instance/{}", escape(id)))?
        else {
            return Ok(None);
        };
        Ok(self.instance_doc(a, p, &o))
    }

    fn instance_doc(&self, a: &Account, p: &str, o: &Json) -> Option<(Json, Json)> {
        if matches!(s(o, "status"), Some("DELETED" | "SOFT_DELETED")) {
            return None;
        }
        let region = s(o, "region").unwrap_or_default();
        if ["flavor", "image"]
            .iter()
            .any(|k| o.get(k).and_then(|x| s(x, "name")).is_none())
        {
            self.list_region(a, p, region);
        }
        let named = |what: &str, embedded: &str, id: &str| {
            o.get(embedded)
                .and_then(|x| s(x, "name"))
                .map(str::to_string)
                .or_else(|| self.name_of(a, p, what, region, s(o, id).unwrap_or_default()))
        };
        let flavor = named("flavor", "flavor", "flavorId");
        let image = named("image", "image", "imageId");
        let nets = self.private_networks(a, p, o);
        Some(map::instance(o, flavor.as_deref(), image.as_deref(), &nets))
    }

    /// Whether an instance's flavor changes from `prior`'s to a smaller
    /// one in `d`, which the API cannot resize to. Not when the account
    /// cannot be reached: Apply refuses it then.
    pub(super) fn flavor_shrinks(&self, at: &str, prior: &Json, d: &Json) -> bool {
        let (Some(was), Some(now), Some(region)) =
            (s(prior, "flavor"), s(d, "flavor"), s(d, "region"))
        else {
            return false;
        };
        if was == now {
            return false;
        }
        let Ok((a, p)) = self.project(at) else {
            return false;
        };
        self.smaller_flavor(&a, &p, region, was, now) == Some(true)
    }

    /// An instance's flavor and image are offered in its region, when the
    /// account is reachable and they are known.
    pub(super) fn check_instance(&self, at: &str, d: &Json) -> Result<()> {
        let Ok((a, p)) = self.project(at) else {
            return Ok(());
        };
        let (Some(region), flavor, image) = (s(d, "region"), s(d, "flavor"), s(d, "image")) else {
            return Ok(());
        };
        if flavor.is_some() && image.is_some() {
            self.list_region(&a, &p, region);
        }
        if let Some(f) = flavor {
            self.flavor_id(&a, &p, region, f)
                .map_err(|e| anyhow!("{at}: flavor: {e:#}"))?;
        }
        if let Some(i) = image {
            self.image_id(&a, &p, region, i)
                .map_err(|e| anyhow!("{at}: image: {e:#}"))?;
        }
        Ok(())
    }

    pub(super) fn create_instance(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let region = need(at, config, "region")?;
        let flavor = need(at, config, "flavor")?;
        let image = need(at, config, "image")?;
        let mut body = json!({
            "name": need(at, config, "name")?,
            "region": region,
            "flavorId": self.flavor_id(&a, &p, region, flavor).map_err(|e| refused(at, format!("{e:#}")))?,
            "imageId": self.image_id(&a, &p, region, image).map_err(|e| refused(at, format!("{e:#}")))?,
            "monthlyBilling": false,
        });
        if config.get("ssh_key").is_some() {
            body["sshKeyId"] = json!(need(at, config, "ssh_key")?);
        }
        let user_data = match config.get("user_data") {
            None => None,
            Some(_) => Some(need(at, config, "user_data")?),
        };
        if let Some(u) = user_data {
            body["userData"] = json!(u);
        }
        if let Some(nets) = self.instance_networks(&a, &p, at, region, config)? {
            body["networks"] = json!(nets);
        }
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/instance"), &body)
            .map_err(|e| failed(at, e))?;
        let id = s(&o, "id").unwrap_or_default().to_string();
        let last = wait_active(&a, &p, at, &id, o, notes, say)?;
        let (attrs, computed) = self
            .instance_doc(&a, &p, &last)
            .ok_or_else(|| Failed::MaybeApplied(format!("{at}: instance {id} was deleted")))?;
        Ok((id, attrs, computed))
    }

    /// An instance's flavor changed in place to a larger one (`POST
    /// /instance/{id}/resize`): the instance reboots into it, and is waited
    /// for until it is ACTIVE on it. A smaller one is refused: the API
    /// resizes only up, and Plan replaces the instance for one.
    #[allow(clippy::too_many_arguments)]
    fn resize(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let flavor = need(at, config, "flavor")?;
        let Some(was) = s(now, "flavor").filter(|w| *w != flavor) else {
            return Ok(());
        };
        let region = need(at, config, "region")?;
        if self.smaller_flavor(a, p, region, was, flavor) == Some(true) {
            return Err(refused(
                at,
                format!(
                    "flavor {flavor} is smaller than {was}, and the API resizes an instance only \
                     to a larger one: plan with the account reachable, which replaces it"
                ),
            ));
        }
        let id = self
            .flavor_id(a, p, region, flavor)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let path = format!("/cloud/project/{p}/instance/{}", escape(remote));
        let o = a
            .client
            .post(&format!("{path}/resize"), &json!({"flavorId": id}))
            .map_err(|e| failed(at, e))?;
        // The instance's flavor is its `flavorId`, or its `flavor`'s id
        // where the API embeds the flavor.
        let on = |o: &Json| {
            s(o, "flavorId").or_else(|| o.get("flavor").and_then(|f| s(f, "id")))
                == Some(id.as_str())
        };
        let resized = |o: &Json| status(o) == "ACTIVE" && on(o);
        self.settle_until(
            a,
            at,
            &path,
            o,
            &resized,
            &["ERROR"],
            CREATE_WAIT,
            notes,
            say,
        )?;
        notes.push(format!(
            "{at}: resized from {was} to {flavor}; the instance rebooted"
        ));
        Ok(())
    }

    /// The instance named `name` in `region`, unless it is deleted.
    pub(super) fn find_instance(&self, name: &str, region: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find an instance")?;
        let list = a.client.get(&format!(
            "/cloud/project/{p}/instance?region={}",
            escape(region)
        ))?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| !matches!(s(o, "status"), Some("DELETED" | "SOFT_DELETED")))
            .find(|o| s(o, "name") == Some(name) && s(o, "region") == Some(region))
            .and_then(|o| s(o, "id"))
            .map(str::to_string))
    }

    /// An instance renamed, resized and its networks brought to `config`;
    /// `now` its document as read.
    pub(super) fn update_instance(
        &self,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let name = need(at, config, "name")?;
        if s(now, "name") != Some(name) {
            a.client
                .put(
                    &format!("/cloud/project/{p}/instance/{}", escape(remote)),
                    &json!({"instanceName": name}),
                )
                .map_err(|e| failed(at, e))?;
        }
        self.resize(&a, &p, at, remote, now, config, notes, say)?;
        self.update_instance_networks(at, remote, now, config, notes)
    }

    /// An instance deleted (gone already is done), and waited for until the
    /// API no longer answers it, saying each status it gives.
    pub(super) fn delete_instance(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let path = format!("/cloud/project/{p}/instance/{}", escape(remote));
        match a.client.delete(&path) {
            Ok(_) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(failed(at, e)),
        }
        let start = Instant::now();
        let mut said = None;
        loop {
            let got = a.client.get_opt(&path);
            if let Ok(Some(o)) = &got
                && let Some(now) = s(o, "status")
                && said.as_deref() != Some(now)
            {
                say(now, None);
                said = Some(now.to_string());
            }
            match got {
                Ok(None) => break,
                Ok(Some(o)) if matches!(s(&o, "status"), Some("DELETED")) => break,
                _ if start.elapsed() > DELETE_WAIT => {
                    notes.push(format!(
                        "{at}: instance {remote} is still being deleted after {}s",
                        DELETE_WAIT.as_secs()
                    ));
                    break;
                }
                _ => std::thread::sleep(a.poll),
            }
        }
        Ok(())
    }
}

/// Wait for instance `id` to run (until then it has no address), `o` as
/// the API last answered it: its document then, or as it was when it
/// failed or the wait ran out, with a note.
fn wait_active(
    a: &Account,
    p: &str,
    at: &str,
    id: &str,
    o: Json,
    notes: &mut Vec<String>,
    say: Say,
) -> std::result::Result<Json, Failed> {
    // Wait for it to run: until then it has no address. Each status
    // the API gives that is not the last one's is said (`BUILD`,
    // `ACTIVE`).
    let start = Instant::now();
    let status = |o: &Json| s(o, "status").unwrap_or("unknown").to_string();
    let mut said = status(&o);
    say(&said, None);
    let mut last = o;
    while s(&last, "status") != Some("ACTIVE") {
        if s(&last, "status") == Some("ERROR") {
            notes.push(format!(
                "{at}: instance {id} is in ERROR; it is kept in state, to be replaced or \
                 deleted"
            ));
            break;
        }
        if start.elapsed() > CREATE_WAIT {
            notes.push(format!(
                "{at}: instance {id} is still {} after {}s",
                s(&last, "status").unwrap_or("unknown"),
                CREATE_WAIT.as_secs()
            ));
            break;
        }
        std::thread::sleep(a.poll);
        match a
            .client
            .get_opt(&format!("/cloud/project/{p}/instance/{}", escape(id)))
        {
            Ok(Some(o)) => {
                if status(&o) != said {
                    said = status(&o);
                    say(&said, None);
                }
                last = o;
            }
            Ok(None) => {
                return Err(Failed::MaybeApplied(format!(
                    "{at}: instance {id} went away while it was made"
                )));
            }
            // A failed poll is no answer about the instance: ask again.
            Err(e) => say(&said, Some(&format!("a poll failed, asking again: {e:#}"))),
        }
    }
    Ok(last)
}
