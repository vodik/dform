//! Health (R-203): an instance's, a volume's, a private network's and a
//! user's status word as the API answers it now, for `dform status` and
//! nothing else; one Read each. Progressing is on its way (`BUILD`,
//! `creating`), degraded is failing and not getting better by itself
//! (`ERROR`, a volume's `error_*`) or gone, suspended is stopped on
//! purpose (`SHUTOFF`, `SHELVED`). The other types (an SSH key, a record,
//! a container, a subnet) have no status to judge: they are there or not.

use super::{INSTANCE, NETWORK, Ovh, USER, VOLUME};
use anyhow::Result;
use dform_core::plugin::backend::health;
use dform_core::plugin::pb::{self, HealthState as S};
use serde_json::Value as Json;

/// The types the provider answers Health for.
pub const TYPES: [&str; 4] = [INSTANCE, VOLUME, NETWORK, USER];

impl Ovh {
    /// Each object's health, from its status as Read answers it now.
    pub fn health(&self, objects: &[pb::Identity]) -> Result<Vec<pb::Health>> {
        objects
            .iter()
            .map(|o| {
                let read = self.read(&o.r#type, &o.remote, &o.name)?;
                Ok(judge(
                    &o.r#type,
                    read.as_ref().map(|(_, computed)| computed),
                ))
            })
            .collect()
    }
}

/// An object of `typ` whose computed values are `computed`; `None` (it is
/// gone) is degraded.
pub fn judge(typ: &str, computed: Option<&Json>) -> pb::Health {
    let Some(c) = computed else {
        return health(S::Degraded, "not found");
    };
    let status = c.get("status").and_then(Json::as_str).unwrap_or_default();
    let state = match typ {
        INSTANCE => instance(status),
        VOLUME => volume(status),
        NETWORK => return network(status, c.get("regions_status")),
        USER => user(status),
        _ => S::Unknown,
    };
    health(state, status)
}

/// A Public Cloud instance's status (OpenStack's server statuses).
fn instance(status: &str) -> S {
    match status {
        "ACTIVE" => S::Healthy,
        "BUILD" | "BUILDING" | "REBUILD" | "RESIZE" | "VERIFY_RESIZE" | "REVERT_RESIZE"
        | "REBOOT" | "HARD_REBOOT" | "MIGRATING" | "PASSWORD" | "RESCUING" | "UNRESCUING"
        | "SHELVING" | "UNSHELVING" | "SNAPSHOTTING" | "RESUMING" => S::Progressing,
        "ERROR" | "DELETED" | "SOFT_DELETED" => S::Degraded,
        "SHUTOFF" | "STOPPED" | "SHELVED" | "SHELVED_OFFLOADED" | "PAUSED" | "SUSPENDED"
        | "RESCUE" => S::Suspended,
        _ => S::Unknown,
    }
}

/// A block storage volume's status (Cinder's).
fn volume(status: &str) -> S {
    match status {
        "available" | "in-use" => S::Healthy,
        s if s.starts_with("error") => S::Degraded,
        "creating" | "attaching" | "detaching" | "extending" | "downloading" | "uploading"
        | "reserved" | "retyping" | "maintenance" | "backing-up" | "restoring-backup"
        | "awaiting-transfer" => S::Progressing,
        _ => S::Unknown,
    }
}

fn user(status: &str) -> S {
    match status {
        "ok" => S::Healthy,
        "creating" => S::Progressing,
        "deleting" | "deleted" => S::Degraded,
        _ => S::Unknown,
    }
}

/// A private network's status, and each region's: the least healthy
/// region's, named, when one is not ACTIVE.
fn network(status: &str, regions: Option<&Json>) -> pb::Health {
    let word = |s: &str| match s {
        "ACTIVE" => S::Healthy,
        "BUILDING" | "BUILD" => S::Progressing,
        "ERROR" | "DOWN" => S::Degraded,
        _ => S::Unknown,
    };
    let rank = |s: S| match s {
        S::Healthy => 0,
        S::Progressing => 1,
        S::Unknown | S::Unspecified | S::Suspended => 2,
        S::Degraded => 3,
    };
    let mut worst = (word(status), status.to_string());
    for (region, s) in regions
        .and_then(Json::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(r, s)| Some((r, s.as_str()?)))
    {
        if rank(word(s)) > rank(worst.0) {
            worst = (word(s), format!("{s} in {region}"));
        }
    }
    health(worst.0, worst.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(typ: &str, computed: Json) -> (S, String) {
        let h = judge(typ, Some(&computed));
        (S::try_from(h.state).unwrap(), h.reason)
    }

    #[test]
    fn an_instance_is_judged_by_its_status_word() {
        let s = |w: &str| state(INSTANCE, json!({"status": w})).0;
        assert_eq!(
            ["ACTIVE", "BUILD", "RESIZE", "ERROR", "SHUTOFF", "WHATEVER"].map(s),
            [
                S::Healthy,
                S::Progressing,
                S::Progressing,
                S::Degraded,
                S::Suspended,
                S::Unknown
            ]
        );
        assert_eq!(
            state(INSTANCE, json!({"status": "ERROR"})).1,
            "ERROR",
            "the reason is the word"
        );
    }

    #[test]
    fn a_volume_a_user_and_a_network_are_judged_by_theirs() {
        let v = |w: &str| state(VOLUME, json!({"status": w})).0;
        assert_eq!(
            ["available", "in-use", "creating", "error_extending"].map(v),
            [S::Healthy, S::Healthy, S::Progressing, S::Degraded]
        );
        let u = |w: &str| state(USER, json!({"status": w})).0;
        assert_eq!(["ok", "creating"].map(u), [S::Healthy, S::Progressing]);
        assert_eq!(
            state(
                NETWORK,
                json!({"status": "ACTIVE", "regions_status": {"BHS5": "ACTIVE", "GRA11": "BUILDING"}})
            ),
            (S::Progressing, "BUILDING in GRA11".into())
        );
        assert_eq!(
            state(
                NETWORK,
                json!({"status": "ACTIVE", "regions_status": {"BHS5": "ACTIVE"}})
            )
            .0,
            S::Healthy
        );
    }

    #[test]
    fn an_object_that_is_gone_is_degraded() {
        let h = judge(INSTANCE, None);
        assert_eq!(S::try_from(h.state).unwrap(), S::Degraded);
    }
}
