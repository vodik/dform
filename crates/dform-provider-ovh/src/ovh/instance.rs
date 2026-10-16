//! Instances (`ovh.instance`): the API's `/cloud/project/{p}/instance/{id}`,
//! named by their name in a region. A Create waits for the instance to be
//! ACTIVE, saying each status the API gives (`BUILD`, `ACTIVE`); its flavor
//! and image are checked against what the region offers (`catalog`). A
//! larger flavor resizes it in place (it reboots), a smaller one replaces it;
//! its networks are interfaces (`network`). A Delete waits for it to go.

use super::*;

impl Ovh {
    /// The instance `id`, its ports and the project's private networks
    /// (which name the ports' networks, listed once per run) asked at
    /// once: none waits on another, and the API is seconds away.
    pub(super) async fn read_instance(
        &self,
        a: &Account,
        p: &str,
        id: &str,
        owned: &map::Owned,
    ) -> api::Result<Option<(Json, Json)>> {
        let (path, ports) = (instance_path(p, id), network::interface_path(p, id));
        let (o, ifaces, networks) = dform_sdk::join3(
            a.client.get_opt(&path),
            a.client.get(&ports),
            self.private_networks(a, p),
        )
        .await;
        let Some(o) = o? else {
            return Ok(None);
        };
        let ifaces = ifaces?;
        networks?;
        self.instance_doc(a, p, &o, &ifaces, owned).await
    }

    /// An instance's document, `ifaces` its ports as the API lists them,
    /// `owned` which are its own; a call that fails fails it, rather than
    /// leave out what Plan would read as changed (an image left out would
    /// replace the instance, a network attach it again).
    async fn instance_doc(
        &self,
        a: &Account,
        p: &str,
        o: &Json,
        ifaces: &Json,
        owned: &map::Owned,
    ) -> api::Result<Option<(Json, Json)>> {
        if matches!(s(o, "status"), Some("DELETED" | "SOFT_DELETED")) {
            return Ok(None);
        }
        let region = s(o, "region").unwrap_or_default();
        if ["flavor", "image"]
            .iter()
            .any(|k| o.get(k).and_then(|x| s(x, "name")).is_none())
        {
            self.list_region(a, p, region).await;
        }
        let named = async |what: &str, embedded: &str, id: &str| match o
            .get(embedded)
            .and_then(|x| s(x, "name"))
        {
            Some(n) => Ok(Some(n.to_string())),
            None => match s(o, id) {
                Some(id) => self.name_of(a, p, what, region, id).await,
                None => Ok(None),
            },
        };
        let flavor = named("flavor", "flavor", "flavorId").await?;
        let image = named("image", "image", "imageId").await?;
        let nics = self.ports(a, p, ifaces).await?;
        Ok(Some(map::instance(
            o,
            flavor.as_deref(),
            image.as_deref(),
            &nics,
            owned,
        )))
    }

    /// Why an instance is replaced when its flavor changes from
    /// `prior`'s to a smaller one in `d`, which the API cannot resize to.
    /// None when the account cannot be reached: Apply refuses it then.
    pub(super) async fn flavor_shrinks(&self, at: &str, prior: &Json, d: &Json) -> Option<String> {
        let (was, now, region) = (s(prior, "flavor")?, s(d, "flavor")?, s(d, "region")?);
        if was == now {
            return None;
        }
        let (a, p) = self.project(at).ok()?;
        (self.smaller_flavor(&a, &p, region, was, now).await == Some(true)).then(|| {
            format!(
                "flavor {now} is smaller than {was}; the API resizes an instance only to a \
                 larger one"
            )
        })
    }

    /// An instance's flavor and image are offered in its region, when the
    /// account is reachable and they are known, and `prior` (the instance
    /// as Read answered it) has not them there already.
    pub(super) async fn check_instance(
        &self,
        at: &str,
        prior: Option<&Json>,
        d: &Json,
    ) -> Result<()> {
        let Ok((a, p)) = self.project(at) else {
            return Ok(());
        };
        let asked = |k: &str| s(d, k).filter(|_| changed(prior, d, k));
        let (Some(region), flavor, image) = (s(d, "region"), asked("flavor"), asked("image"))
        else {
            return Ok(());
        };
        if flavor.is_some() && image.is_some() {
            self.list_region(&a, &p, region).await;
        }
        if let Some(f) = flavor {
            self.flavor_id(&a, &p, region, f)
                .await
                .map_err(|e| anyhow!("{at}: flavor: {e:#}"))?;
        }
        if let Some(i) = image {
            self.image_id(&a, &p, region, i)
                .await
                .map_err(|e| anyhow!("{at}: image: {e:#}"))?;
        }
        Ok(())
    }

    pub(super) async fn create_instance(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say<'_>,
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
            "flavorId": self.flavor_id(&a, &p, region, flavor).await.map_err(|e| refused(at, format!("{e:#}")))?,
            "imageId": self.image_id(&a, &p, region, image).await.map_err(|e| refused(at, format!("{e:#}")))?,
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
        if let Some(nets) = self.instance_networks(&a, &p, at, region, config).await? {
            body["networks"] = json!(nets);
        }
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/instance"), Some(&body))
            .await
            .map_err(|e| failed(at, e))?;
        let id = s(&o, "id").unwrap_or_default().to_string();
        let last = wait_active(&a, &p, at, &id, o, notes, say).await?;
        let owned = map::Owned::made(network::fixed(config));
        let made = |e| Failed::MaybeApplied(format!("{at}: instance {id} is made: {e}"));
        let ifaces = a
            .client
            .get(&network::interface_path(&p, &id))
            .await
            .map_err(made)?;
        let (attrs, computed) = self
            .instance_doc(&a, &p, &last, &ifaces, &owned)
            .await
            .map_err(made)?
            .ok_or_else(|| Failed::MaybeApplied(format!("{at}: instance {id} was deleted")))?;
        Ok((id, attrs, computed))
    }

    /// The computed values of instance `remote`, which a Create of
    /// `config` made, as its prior: the port on each network it was made
    /// on is its own (`map::Owned::made`), with the address `config`
    /// asked there, for the Create sent again that finds it.
    pub(super) async fn made_instance(
        &self,
        at: &str,
        remote: &str,
        config: &Json,
    ) -> std::result::Result<Option<Json>, Failed> {
        let (a, p) = self.project_for(at)?;
        let owned = map::Owned::made(network::fixed(config));
        let made = self
            .read_instance(&a, &p, remote, &owned)
            .await
            .map_err(|e| failed(at, e))?;
        Ok(made.map(|(_, computed)| computed))
    }

    /// An instance's flavor changed in place to a larger one (`POST
    /// /instance/{id}/resize`): the instance reboots into it, and is waited
    /// for until it is ACTIVE on it. A smaller one is refused: the API
    /// resizes only up, and Plan replaces the instance for one.
    #[allow(clippy::too_many_arguments)]
    async fn resize(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say<'_>,
    ) -> std::result::Result<(), Failed> {
        let flavor = need(at, config, "flavor")?;
        let Some(was) = s(now, "flavor").filter(|w| *w != flavor) else {
            return Ok(());
        };
        let region = need(at, config, "region")?;
        if self.smaller_flavor(a, p, region, was, flavor).await == Some(true) {
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
            .await
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let path = instance_path(p, remote);
        let o = a
            .client
            .post(&format!("{path}/resize"), Some(&json!({"flavorId": id})))
            .await
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
        )
        .await?;
        notes.push(format!(
            "{at}: resized from {was} to {flavor}; the instance rebooted"
        ));
        Ok(())
    }

    /// The instance named `name` in `region`, unless it is deleted.
    pub(super) async fn find_instance(&self, name: &str, region: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find an instance")?;
        let list = a
            .client
            .list(&format!(
                "/cloud/project/{p}/instance?region={}",
                escape(region)
            ))
            .await?;
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
    /// `now` its document as read, `owned` which of its ports are its own.
    /// Its own port on each network now, by network. A network whose ports
    /// none is known as its own refuses it before anything changes.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn update_instance(
        &self,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
        owned: &map::Owned,
        notes: &mut Vec<String>,
        say: Say<'_>,
    ) -> std::result::Result<Vec<(String, String)>, Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let ports = self
            .instance_ports(&a, &p, at, remote, config, owned)
            .await?;
        let name = need(at, config, "name")?;
        if s(now, "name") != Some(name) {
            a.client
                .put(&instance_path(&p, remote), &json!({"instanceName": name}))
                .await
                .map_err(|e| failed(at, e))?;
        }
        self.resize(&a, &p, at, remote, now, config, notes, say)
            .await?;
        self.converge_ports(&a, &p, at, remote, config, ports, notes)
            .await
    }

    /// An instance deleted (gone already is done), and waited for until the
    /// API no longer answers it, saying each status it gives.
    pub(super) async fn delete_instance(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say<'_>,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let path = instance_path(&p, remote);
        match a.client.delete(&path).await {
            Ok(_) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(failed(at, e)),
        }
        let start = Instant::now();
        let mut said = None;
        loop {
            let got = a.client.get_opt(&path).await;
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
                _ => dform_sdk::sleep(a.poll).await,
            }
        }
        Ok(())
    }
}

/// Wait for instance `id` to run (until then it has no address), `o` as
/// the API last answered it: its document then, or as it was when it
/// failed or the wait ran out, with a note.
async fn wait_active(
    a: &Account,
    p: &str,
    at: &str,
    id: &str,
    o: Json,
    notes: &mut Vec<String>,
    say: Say<'_>,
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
        dform_sdk::sleep(a.poll).await;
        match a.client.get_opt(&instance_path(p, id)).await {
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

/// `/instance/{remote}` of project `p`.
fn instance_path(p: &str, remote: &str) -> String {
    format!("/cloud/project/{p}/instance/{}", escape(remote))
}
