//! Block volumes: made (creating then available), renamed, attached
//! (attaching then in-use), detached, grown, deleted.

use super::*;

impl World {
    /// A call under the volume routes.
    pub(super) fn volume_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        query: &BTreeMap<String, String>,
        body: &Json,
    ) -> (u16, Json) {
        let q = |k: &str| query.get(k).map(String::as_str);
        match (method, segs) {
            // Volumes.
            ("GET", ["cloud", "project", _, "volume"]) => {
                let all: Vec<Json> = self
                    .volumes
                    .values()
                    .filter(|v| q("region").is_none_or(|r| v["region"] == r))
                    .cloned()
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "volume"]) => {
                let s = |k: &str| body.get(k).and_then(Json::as_str).unwrap_or_default();
                let region = s("region");
                if !REGIONS.contains(&region) {
                    return (400, json!({"message": format!("Invalid region {region}")}));
                }
                let typ = match s("type") {
                    "" => "classic",
                    t => t,
                };
                let types = [
                    "classic",
                    "classic-luks",
                    "high-speed",
                    "high-speed-luks",
                    "high-speed-gen2",
                    "high-speed-gen2-luks",
                ];
                if !types.contains(&typ) {
                    return (
                        400,
                        json!({"message": format!("Invalid volume type {typ}")}),
                    );
                }
                let size = body.get("size").and_then(Json::as_i64).unwrap_or(0);
                if size < 1 {
                    return (400, json!({"message": "size must be at least 1 GiB"}));
                }
                let image = s("imageId");
                let images = Self::images(region);
                if !image.is_empty()
                    && !images
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|i| i["id"] == image)
                {
                    return (400, json!({"message": "Invalid imageId"}));
                }
                let id = self.id("volume");
                let mut o = volume(
                    &id,
                    s("name"),
                    region,
                    size,
                    typ,
                    s("description"),
                    !image.is_empty(),
                );
                o["status"] = json!("creating");
                self.volumes.insert(id.clone(), o.clone());
                self.settle(format!("volume/{id}"), json!({"status": "available"}));
                (200, o)
            }
            (m, ["cloud", "project", _, "volume", id, rest @ ..]) => {
                let id = id.to_string();
                if m == "GET" && rest.is_empty() {
                    self.read_settling(&format!("volume/{id}"));
                }
                let Some(v) = self.volumes.get(&id).cloned() else {
                    return not_found("Volume");
                };
                let status = v["status"].as_str().unwrap_or_default().to_string();
                let instance = body
                    .get("instanceId")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                let refuse = |why: String| (400, json!({"message": why}));
                match (m, rest) {
                    ("GET", []) => (200, v),
                    ("PUT", []) => {
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        for k in ["name", "description"] {
                            if let Some(x) = body.get(k) {
                                o[k] = x.clone();
                            }
                        }
                        (200, o.clone())
                    }
                    ("DELETE", []) if status == "in-use" => {
                        refuse(format!("Volume {id} is attached to an instance"))
                    }
                    ("DELETE", []) => {
                        self.volumes.get_mut(&id).expect("looked up")["status"] = json!("deleting");
                        self.settle(format!("volume/{id}"), Json::Null);
                        (200, Json::Null)
                    }
                    ("POST", ["attach"]) => {
                        if status != "available" {
                            return refuse(format!("Volume {id} is {status}, not available"));
                        }
                        let Some(i) = self.instances.get(instance) else {
                            return not_found("Instance");
                        };
                        if i["region"] != v["region"] {
                            return refuse("The instance is in another region".into());
                        }
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        o["status"] = json!("attaching");
                        let o = o.clone();
                        self.settle(
                            format!("volume/{id}"),
                            json!({"status": "in-use", "attachedTo": [instance]}),
                        );
                        (200, o)
                    }
                    ("POST", ["detach"]) => {
                        if !v["attachedTo"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|x| x == instance)
                        {
                            return refuse(format!("Volume {id} is not attached to {instance}"));
                        }
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        o["status"] = json!("detaching");
                        let o = o.clone();
                        self.settle(
                            format!("volume/{id}"),
                            json!({"status": "available", "attachedTo": []}),
                        );
                        (200, o)
                    }
                    ("POST", ["upsize"]) => {
                        let size = body.get("size").and_then(Json::as_i64).unwrap_or(0);
                        if size <= v["size"].as_i64().unwrap_or(0) {
                            return refuse(
                                "The new size must be greater than the current one".into(),
                            );
                        }
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        o["status"] = json!("extending");
                        o["size"] = json!(size);
                        let o = o.clone();
                        self.settle(format!("volume/{id}"), json!({"status": status}));
                        (200, o)
                    }
                    _ => (404, json!({"message": format!("no route {method} {path}")})),
                }
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}

/// A volume (`cloud.volume.Volume`), available.
pub(super) fn volume(
    id: &str,
    name: &str,
    region: &str,
    gib: i64,
    typ: &str,
    desc: &str,
    boot: bool,
) -> Json {
    json!({"id": id, "name": name, "region": region, "size": gib, "type": typ,
           "description": desc, "status": "available", "attachedTo": [],
           "bootable": boot, "availabilityZone": "nova",
           "creationDate": "2026-10-07T00:00:00Z",
           "planCode": format!("volume.{typ}.consumption")})
}
