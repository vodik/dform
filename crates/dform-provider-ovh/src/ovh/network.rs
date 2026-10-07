//! Private networks (`ovh.network`): the API's
//! `/cloud/project/{p}/network/private/{id}`, a VLAN of the vRack the
//! project is on, in some of its regions. The only network a program
//! makes: the public one is OVH's. The vRack itself is the account's,
//! attached to the project outside dform (as a DNS zone is the
//! account's): a project without one is refused at Plan naming it, not
//! with the API's text. A network is named by its name; it is added to a
//! region in place (`POST /region`), and leaves none (another VLAN or
//! fewer regions replace it).

use super::*;

impl Ovh {
    pub(super) fn read_network(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<Json>> {
        a.client.get_opt(&network_path(p, remote))
    }

    /// Whether the project is on a vRack, asked once; a project that is
    /// not refuses a private network naming it. An account that cannot be
    /// reached is not judged here.
    pub(super) fn check_vrack(&self, at: &str) -> Result<()> {
        let Ok((a, p)) = self.project(at) else {
            return Ok(());
        };
        let known = self
            .vrack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&p)
            .copied();
        let on = match known {
            Some(on) => on,
            None => {
                let on = a
                    .client
                    .get_opt(&format!("/cloud/project/{p}/vrack"))?
                    .is_some();
                self.vrack
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(p.clone(), on);
                on
            }
        };
        if !on {
            bail!("{at}: {}", no_vrack(&p));
        }
        Ok(())
    }

    pub(super) fn create_network(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let mut body = json!({"name": need(at, config, "name")?});
        if let Some(rs) = regions(at, config)? {
            body["regions"] = json!(rs);
        }
        if let Some(v) = config.get("vlan_id") {
            body["vlanId"] = v
                .as_i64()
                .map(Json::from)
                .ok_or_else(|| refused(at, format!("vlan_id {v} is not a number")))?;
        }
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/network/private"), &body)
            .map_err(|e| match e.status() {
                // The API's refusal of a project with no vRack is said as
                // what it is.
                Some(400..500)
                    if matches!(
                        a.client.get_opt(&format!("/cloud/project/{p}/vrack")),
                        Ok(None)
                    ) =>
                {
                    refused(at, no_vrack(&p))
                }
                _ => failed(at, e),
            })?;
        let id = s(&o, "id").unwrap_or_default().to_string();
        let path = network_path(&p, &id);
        let o = self.settle(
            &a,
            at,
            &path,
            o,
            &["ACTIVE"],
            &["ERROR"],
            CREATE_WAIT,
            notes,
            say,
        )?;
        let (attrs, computed) = map::network(&o);
        Ok((id, attrs, computed))
    }

    /// Its name in place, and the regions the program adds.
    pub(super) fn update_network(
        &self,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = network_path(&p, remote);
        let name = need(at, config, "name")?;
        if s(now, "name") != Some(name) {
            a.client
                .put(&path, &json!({"name": name}))
                .map_err(|e| failed(at, e))?;
        }
        let has: Vec<&str> = now
            .get("regions")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        let mut last = None;
        for r in regions(at, config)?.into_iter().flatten() {
            if !has.contains(&r.as_str()) {
                last = Some(
                    a.client
                        .post(&format!("{path}/region"), &json!({"region": r}))
                        .map_err(|e| failed(at, e))?,
                );
            }
        }
        if let Some(o) = last {
            self.settle(
                &a,
                at,
                &path,
                o,
                &["ACTIVE"],
                &["ERROR"],
                CREATE_WAIT,
                notes,
                say,
            )?;
        }
        Ok(())
    }
}

fn network_path(p: &str, remote: &str) -> String {
    format!("/cloud/project/{p}/network/private/{}", escape(remote))
}

/// Why a private network cannot be made in project `p`.
fn no_vrack(p: &str) -> String {
    format!(
        "project {p} is not on a vRack, and a private network is a VLAN of one: attach the \
         account's vRack to the project in the OVH control panel (it is the account's, \
         outside dform)"
    )
}

/// The program's regions, if it names them.
fn regions(at: &str, config: &Json) -> std::result::Result<Option<Vec<String>>, Failed> {
    match config.get("regions") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Array(rs)) => rs
            .iter()
            .map(|r| {
                r.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| refused(at, format!("regions: {r} is not a region's name")))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some),
        Some(v) => Err(refused(
            at,
            format!(
                "regions is {}, not a list of names",
                provider::fmt_value(Some(v))
            ),
        )),
    }
}

/// Subnets (`ovh.subnet`): the API's
/// `/cloud/project/{p}/network/private/{id}/subnet/{subnet}`, a range of a
/// private network in one region and the pool of it OpenStack hands out.
/// It is its own object (it has an id), named by its network, region and
/// range; nothing of it changes in place.
impl Ovh {
    /// A network's subnets; none when the network is gone.
    pub(super) fn subnets(&self, a: &Account, p: &str, network: &str) -> api::Result<Vec<Json>> {
        Ok(a.client
            .get_opt(&format!("{}/subnet", network_path(p, network)))?
            .and_then(|l| l.as_array().cloned())
            .unwrap_or_default())
    }

    /// The API has no GET of one subnet: its network's are listed.
    pub(super) fn read_subnet(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<Json>> {
        let (network, id) = map::subnet_parts(remote);
        Ok(self
            .subnets(a, p, network)?
            .into_iter()
            .find(|o| s(o, "id") == Some(id)))
    }

    pub(super) fn create_subnet(
        &self,
        at: &str,
        config: &Json,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let network = need(at, config, "network")?;
        let flag = |k: &str| config.get(k).and_then(Json::as_bool).unwrap_or(false);
        let body = json!({
            "region": need(at, config, "region")?,
            "network": need(at, config, "range")?,
            "start": need(at, config, "start")?,
            "end": need(at, config, "end")?,
            "dhcp": flag("dhcp"),
            "noGateway": flag("no_gateway"),
        });
        let o = a
            .client
            .post(&format!("{}/subnet", network_path(&p, network)), &body)
            .map_err(|e| failed(at, e))?;
        let (attrs, computed) = map::subnet(network, &o);
        let id = map::subnet_remote(network, s(&o, "id").unwrap_or_default());
        Ok((id, attrs, computed))
    }
}
