//! SSH keys: listed, made, read and deleted.

use super::*;

impl World {
    /// A call under the ssh key routes.
    pub(super) fn ssh_key_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        body: &Json,
    ) -> (u16, Json) {
        match (method, segs) {
            ("GET", ["cloud", "project", _, "sshkey"]) => {
                (200, json!(self.keys.values().cloned().collect::<Vec<_>>()))
            }
            ("POST", ["cloud", "project", _, "sshkey"]) => {
                let id = self.id("key");
                let o = json!({"id": id, "name": body["name"], "publicKey": body["publicKey"],
                               "fingerPrint": "00:11", "regions": REGIONS});
                self.keys.insert(id, o.clone());
                (200, o)
            }
            ("GET", ["cloud", "project", _, "sshkey", id]) => self
                .keys
                .get(*id)
                .cloned()
                .map_or_else(|| not_found("SSH key"), |o| (200, o)),
            ("DELETE", ["cloud", "project", _, "sshkey", id]) => match self.keys.remove(*id) {
                Some(_) => (200, Json::Null),
                None => not_found("SSH key"),
            },
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}
