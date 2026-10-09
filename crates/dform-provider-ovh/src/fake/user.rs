//! Users (creating then ok) with their roles, and their S3 credentials.

use super::*;

impl World {
    /// A call under the user routes.
    pub(super) fn user_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        body: &Json,
    ) -> (u16, Json) {
        match (method, segs) {
            // Users and their S3 credentials.
            ("GET", ["cloud", "project", _, "role"]) => (
                200,
                json!({"roles": ROLES.iter().map(|r| role(r)).collect::<Vec<_>>(),
                       "services": []}),
            ),
            ("GET", ["cloud", "project", _, "user"]) => {
                let all: Vec<Json> = self
                    .users
                    .values()
                    .map(|u| {
                        let mut u = u.clone();
                        u.as_object_mut().map(|m| m.remove("password"));
                        u
                    })
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "user"]) => {
                let mut names: Vec<String> = body
                    .get("roles")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_str().map(str::to_string))
                    .collect();
                names.extend(body.get("role").and_then(Json::as_str).map(str::to_string));
                if let Some(r) = names.iter().find(|r| !ROLES.contains(&r.as_str())) {
                    return (400, json!({"message": format!("Invalid role {r}")}));
                }
                self.next += 1;
                let id = 100_000 + self.next as i64;
                let o = json!({"id": id, "username": format!("user-{id:x}"),
                               "description": body["description"], "status": "creating",
                               "creationDate": "2026-10-07T00:00:00Z",
                               "openstackId": format!("os-{id}"),
                               "roles": names.iter().map(|r| role(r)).collect::<Vec<_>>(),
                               "password": format!("pw-{id}")});
                self.users.insert(id, o.clone());
                self.settle(format!("user/{id}"), json!({"status": "ok"}));
                (200, o)
            }
            (m, ["cloud", "project", _, "user", id, rest @ ..]) => {
                let Ok(id) = id.parse::<i64>() else {
                    return not_found("User");
                };
                if m == "GET" && rest.is_empty() {
                    self.read_settling(&format!("user/{id}"));
                }
                let Some(u) = self.users.get(&id) else {
                    return not_found("User");
                };
                let ready = u["status"] == "ok";
                match (m, rest) {
                    ("GET", []) => {
                        let mut u = u.clone();
                        u.as_object_mut().map(|m| m.remove("password"));
                        (200, u)
                    }
                    ("DELETE", []) => {
                        self.users.remove(&id);
                        self.s3.remove(&id);
                        (200, Json::Null)
                    }
                    ("PUT", ["role"]) => {
                        let ids: Vec<&str> = body["rolesIds"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Json::as_str)
                            .collect();
                        let roles: Option<Vec<Json>> = ids
                            .iter()
                            .map(|i| {
                                ROLES
                                    .iter()
                                    .find(|r| format!("role-{r}") == *i)
                                    .map(|r| role(r))
                            })
                            .collect();
                        let Some(roles) = roles else {
                            return (400, json!({"message": "Invalid role id"}));
                        };
                        let u = self.users.get_mut(&id).expect("looked up");
                        u["roles"] = json!(roles);
                        (200, u.clone())
                    }
                    ("GET", ["s3Credentials"]) => (
                        200,
                        json!(
                            self.s3
                                .get(&id)
                                .into_iter()
                                .flatten()
                                .map(|(a, _)| json!({"access": a, "userId": format!("os-{id}"),
                                                 "tenantId": PROJECT}))
                                .collect::<Vec<_>>()
                        ),
                    ),
                    ("POST", ["s3Credentials"]) if !ready => {
                        (400, json!({"message": format!("User {id} is not ready")}))
                    }
                    ("POST", ["s3Credentials"]) => {
                        self.next += 1;
                        let access = format!("AK{:014}", self.next);
                        let secret = format!("sk-{id}-{}", self.next);
                        self.s3
                            .entry(id)
                            .or_default()
                            .push((access.clone(), secret.clone()));
                        (
                            200,
                            json!({"access": access, "secret": secret,
                                   "userId": format!("os-{id}"), "tenantId": PROJECT}),
                        )
                    }
                    (m, ["s3Credentials", access, tail @ ..]) => {
                        let creds = self.s3.entry(id).or_default();
                        let Some(i) = creds.iter().position(|(a, _)| a == access) else {
                            return not_found("Credential");
                        };
                        match (m, tail) {
                            ("POST", ["secret"]) => (200, json!({"secret": creds[i].1})),
                            ("DELETE", []) => {
                                creds.remove(i);
                                (200, Json::Null)
                            }
                            _ => (405, json!({"message": "method"})),
                        }
                    }
                    _ => (404, json!({"message": format!("no route {method} {path}")})),
                }
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}

/// The OpenStack roles a user may have (`cloud.user.RoleEnum`).
const ROLES: [&str; 6] = [
    "administrator",
    "compute_operator",
    "network_operator",
    "objectstore_operator",
    "volume_operator",
    "infrastructure_supervisor",
];

/// A role (`cloud.role.Role`).
pub(super) fn role(name: &str) -> Json {
    json!({"id": format!("role-{name}"), "name": name,
           "description": format!("{name} role"), "permissions": []})
}
