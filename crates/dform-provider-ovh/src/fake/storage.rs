//! S3 containers of a region: listed, made for an owner, versioned, deleted.

use super::*;

impl World {
    /// A call under the storage routes.
    pub(super) fn storage_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        body: &Json,
    ) -> (u16, Json) {
        match (method, segs) {
            // S3 containers.
            ("GET", ["cloud", "project", _, "region", r, "storage"]) => {
                let all: Vec<Json> = self
                    .containers
                    .iter()
                    .filter(|((cr, _), _)| cr == r)
                    .map(|(_, c)| c.clone())
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "region", r, "storage"]) => {
                if !REGIONS.contains(r) {
                    return (400, json!({"message": format!("Invalid region {r}")}));
                }
                let name = body["name"].as_str().unwrap_or_default().to_string();
                if self.containers.contains_key(&(r.to_string(), name.clone())) {
                    return (
                        409,
                        json!({"message": format!("Container {name} already exists")}),
                    );
                }
                let owner = match body.get("ownerId").and_then(Json::as_i64) {
                    Some(o) if !self.users.contains_key(&o) => {
                        return (400, json!({"message": format!("Invalid ownerId {o}")}));
                    }
                    Some(o) => o,
                    None => match self.s3.keys().next() {
                        Some(o) => *o,
                        None => {
                            return (400, json!({"message": "No S3 user found for the project"}));
                        }
                    },
                };
                let versioning = body["versioning"]["status"].as_str().unwrap_or("disabled");
                let o = container(r, &name, Some(owner), versioning);
                self.containers.insert((r.to_string(), name), o.clone());
                (200, o)
            }
            (m, ["cloud", "project", _, "region", r, "storage", name]) => {
                let k = (r.to_string(), name.to_string());
                let Some(c) = self.containers.get_mut(&k) else {
                    return not_found("Container");
                };
                match m {
                    "GET" => (200, c.clone()),
                    "PUT" => {
                        if let Some(v) = body["versioning"]["status"].as_str() {
                            if v == "disabled" && c["versioning"]["status"] != "disabled" {
                                return (
                                    400,
                                    json!({"message": "Versioning cannot be disabled once enabled"}),
                                );
                            }
                            c["versioning"]["status"] = json!(v);
                        }
                        (200, c.clone())
                    }
                    "DELETE" => {
                        if c["objectsCount"].as_i64().unwrap_or(0) > 0 {
                            return (409, json!({"message": "The container is not empty"}));
                        }
                        self.containers.remove(&k);
                        (200, Json::Null)
                    }
                    _ => (405, json!({"message": "method"})),
                }
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}

/// An S3 container (`cloud.StorageContainer`), empty.
pub(super) fn container(region: &str, name: &str, owner: Option<i64>, versioning: &str) -> Json {
    let host = format!("{name}.s3.{}.io.cloud.ovh.net", region.to_lowercase());
    json!({"name": name, "region": region, "ownerId": owner,
           "objectsCount": 0, "objectsSize": 0,
           "createdAt": "2026-10-07T00:00:00Z",
           "arn": format!("arn:aws:s3:::{name}"), "virtualHost": host,
           "versioning": {"status": versioning},
           "encryption": {"sseAlgorithm": "plaintext"}, "tags": {},
           "objectLock": {"status": "disabled"}, "objects": []})
}
