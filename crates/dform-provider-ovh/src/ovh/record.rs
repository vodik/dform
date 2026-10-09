//! DNS records (`ovh.domain_record`): the API's `/domain/zone/{zone}/record/{id}`,
//! their remote id `ZONE/ID`, named by their zone, type, subdomain and target.
//! A zone's changes are served once it is refreshed. A zone the account does
//! not host is said so, with where it is delegated (R-125). The zones the
//! account hosts are a data source, `ovh.zone` (R-196).

use super::*;

impl Ovh {
    /// `ovh.zone(+name, -id, -nameservers)`: the zone `name` when the
    /// account hosts it (`GET /domain/zone/{zone}`), its id the name the
    /// API keys it by and its nameservers those the API says serve it;
    /// no row when it does not (a 404). A `not { ovh.zone(..) }` deny reads
    /// that absence; Plan's refusal of a record in such a zone stays.
    pub(super) fn zone_rows(&self, what: &str, name: &str) -> Result<Vec<Vec<Value>>> {
        let a = self.account(what)?;
        Ok(a.client
            .get_opt(&format!("/domain/zone/{}", escape(name)))?
            .map(|z| map::zone_row(name, &z))
            .into_iter()
            .collect())
    }

    pub(super) fn read_record(&self, a: &Account, remote: &str) -> Result<Option<(Json, Json)>> {
        let (zone, id) = remote
            .rsplit_once('/')
            .ok_or_else(|| anyhow!("a record's remote id is ZONE/ID, not {remote:?}"))?;
        Ok(a.client
            .get_opt(&format!(
                "/domain/zone/{}/record/{}",
                escape(zone),
                escape(id)
            ))?
            .map(|o| map::record(&o)))
    }

    /// A zone's changes are served once it is refreshed; a refresh that
    /// fails is a note, the record is written.
    fn refresh_zone(&self, a: &Account, at: &str, zone: &str, notes: &mut Vec<String>) {
        if let Err(e) = a.client.post(
            &format!("/domain/zone/{}/refresh", escape(zone)),
            &json!({}),
        ) {
            notes.push(format!("{at}: the zone {zone} is not refreshed: {e}"));
        }
    }

    /// The record of `typ` at `subdomain` of `zone` pointing to `target`.
    pub(super) fn find_record(
        &self,
        zone_name: &str,
        typ: &str,
        subdomain: &str,
        target: &str,
    ) -> Result<Option<String>> {
        let a = self.account("find a DNS record")?;
        let zone = escape(zone_name);
        let ids = a
            .client
            .get(&format!(
                "/domain/zone/{zone}/record?fieldType={}&subDomain={}",
                escape(typ),
                escape(subdomain)
            ))
            .map_err(|e| match not_hosted(&a, zone_name, &e) {
                Some(m) => anyhow!(m),
                None => e.into(),
            })?;
        let ids: Vec<i64> = ids
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_i64)
            .collect();
        // Each record of the name asked at once: a name with
        // several waits one round trip, not one per record.
        let records: Vec<api::Result<Option<Json>>> = std::thread::scope(|scope| {
            let asks: Vec<_> = ids
                .iter()
                .map(|id| {
                    let (client, zone) = (&a.client, &zone);
                    scope.spawn(move || client.get_opt(&format!("/domain/zone/{zone}/record/{id}")))
                })
                .collect();
            asks.into_iter()
                .map(|h| h.join().expect("a record's GET does not panic"))
                .collect()
        });
        for (id, o) in ids.into_iter().zip(records) {
            if o?.is_some_and(|o| s(&o, "target") == Some(target)) {
                return Ok(Some(map::record_remote(zone_name, id)));
            }
        }
        Ok(None)
    }

    pub(super) fn create_record(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let a = self
            .account(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let zone = need(at, config, "zone")?;
        let mut body = json!({
            "fieldType": need(at, config, "type")?,
            "subDomain": s(config, "subdomain").unwrap_or_default(),
            "target": need(at, config, "target")?,
        });
        if let Some(ttl) = config.get("ttl").and_then(Json::as_i64) {
            body["ttl"] = json!(ttl);
        }
        let o = a
            .client
            .post(&format!("/domain/zone/{}/record", escape(zone)), &body)
            .map_err(|e| match not_hosted(&a, zone, &e) {
                Some(m) => refused(at, m),
                None => failed(at, e),
            })?;
        self.refresh_zone(&a, at, zone, notes);
        let (attrs, computed) = map::record(&o);
        let id = o.get("id").and_then(Json::as_i64).unwrap_or(0);
        Ok((map::record_remote(zone, id), attrs, computed))
    }

    /// The API's update takes the subdomain, the target and the ttl
    /// (R-195); the zone and type are the record's. A ttl the program does
    /// not write is left as it is. `now`: the record as read.
    pub(super) fn update_record(
        &self,
        at: &str,
        remote: &str,
        now: &(Json, Json),
        config: &Json,
    ) -> std::result::Result<(), Failed> {
        let a = self
            .account(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let (zone, id) = remote.rsplit_once('/').unwrap_or_default();
        let was = now.1.get("ttl").and_then(Json::as_i64).unwrap_or(0);
        let body = json!({
            "subDomain": s(config, "subdomain").unwrap_or_default(),
            "target": need(at, config, "target")?,
            "ttl": config.get("ttl").and_then(Json::as_i64).unwrap_or(was),
        });
        let had = json!({
            "subDomain": s(&now.0, "subdomain").unwrap_or_default(),
            "target": s(&now.0, "target").unwrap_or_default(),
            "ttl": was,
        });
        if body != had {
            a.client
                .put(
                    &format!("/domain/zone/{}/record/{}", escape(zone), escape(id)),
                    &body,
                )
                .map_err(|e| failed(at, e))?;
            self.refresh_zone(&a, at, zone, &mut Vec::new());
        }
        Ok(())
    }

    /// A record deleted (gone already is done), and its zone refreshed.
    pub(super) fn delete_record(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
    ) -> std::result::Result<(), Failed> {
        let a = self
            .account(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let (zone, id) = remote.rsplit_once('/').unwrap_or_default();
        match a.client.delete(&format!(
            "/domain/zone/{}/record/{}",
            escape(zone),
            escape(id)
        )) {
            Ok(_) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(failed(at, e)),
        }
        self.refresh_zone(&a, at, zone, notes);
        Ok(())
    }
}

/// A 404 under `/domain/zone/{zone}` when the account does not host the
/// zone itself: says so, and where it is delegated when DNS answers.
fn not_hosted(a: &Account, zone: &str, e: &api::Error) -> Option<String> {
    if !e.is_not_found() {
        return None;
    }
    let path = format!("/domain/zone/{}", escape(zone));
    if !matches!(a.client.get_opt(&path), Ok(None)) {
        return None;
    }
    Some(match crate::dns::nameservers(zone) {
        Some(ns) => format!(
            "zone {zone} is not hosted on this OVH account (its nameservers are {})",
            ns.join(", ")
        ),
        None => format!("zone {zone} is not hosted on this OVH account"),
    })
}
