//! `tailscale.user(+tailnet, -login, -role)`: the tailnet's users, a data
//! source (a user signs in through the identity provider; a program does
//! not make one). `login` is the login name (`alice@example.com`), `role`
//! the tailnet role (`owner`, `admin`, `member`, ..).

use crate::Tailscale;
use crate::client::Body;
use dform_sdk::typed::Result;
use serde_json::{Value as Json, json};

pub const USER: &str = "tailscale.user";

pub fn rows(p: &Tailscale, inputs: &[Json]) -> Result<Vec<Vec<Json>>> {
    let tailnet = p.asked(USER, inputs)?;
    let a = p
        .client
        .call("GET", &p.client.tailnet_path("/users"), Body::None)?;
    Ok(a.body["users"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|u| {
            vec![
                json!(tailnet),
                json!(u["loginName"].as_str().unwrap_or_default()),
                json!(u["role"].as_str().unwrap_or_default()),
            ]
        })
        .collect())
}
