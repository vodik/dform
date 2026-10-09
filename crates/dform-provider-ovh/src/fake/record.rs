//! DNS records of a zone: listed by type and subdomain, made, read, changed
//! and deleted; a zone refreshed.

use super::*;

impl World {
    /// A call under the record routes.
    pub(super) fn record_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        query: &BTreeMap<String, String>,
        body: &Json,
    ) -> (u16, Json) {
        let q = |k: &str| query.get(k).map(String::as_str);
        match (method, segs) {
            ("GET", ["domain", "zone", z, "record"]) => {
                let ids: Vec<i64> = self
                    .records
                    .iter()
                    .filter(|(_, r)| r["zone"] == *z)
                    .filter(|(_, r)| q("fieldType").is_none_or(|t| r["fieldType"] == t))
                    .filter(|(_, r)| q("subDomain").is_none_or(|s| r["subDomain"] == s))
                    .map(|(id, _)| *id)
                    .collect();
                (200, json!(ids))
            }
            ("POST", ["domain", "zone", z, "record"]) => {
                self.next += 1;
                let id = 5_000_000_000 + self.next as i64;
                let o = json!({"id": id, "zone": z, "subDomain": body["subDomain"],
                               "fieldType": body["fieldType"], "target": body["target"],
                               "ttl": body.get("ttl").cloned().unwrap_or(json!(0))});
                self.records.insert(id, o.clone());
                (200, o)
            }
            ("POST", ["domain", "zone", _, "refresh"]) => (200, Json::Null),
            (m, ["domain", "zone", z, "record", id]) => {
                let Ok(id) = id.parse::<i64>() else {
                    return not_found("Record");
                };
                match (m, self.records.get_mut(&id)) {
                    (_, None) => not_found("Record"),
                    (_, Some(r)) if r["zone"] != *z => not_found("Record"),
                    ("GET", Some(r)) => (200, r.clone()),
                    // The update takes the subdomain, the target and the
                    // ttl; a record's zone and type are fixed.
                    ("PUT", Some(r)) => {
                        let fields = body.as_object().into_iter().flatten();
                        if let Some((k, _)) = fields
                            .clone()
                            .find(|(k, _)| !matches!(k.as_str(), "subDomain" | "target" | "ttl"))
                        {
                            return (400, json!({"message": format!("Invalid property {k}")}));
                        }
                        for (k, v) in fields {
                            r[k] = v.clone();
                        }
                        (200, Json::Null)
                    }
                    ("DELETE", Some(_)) => {
                        self.records.remove(&id);
                        (200, Json::Null)
                    }
                    _ => (405, json!({"message": "method"})),
                }
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}
