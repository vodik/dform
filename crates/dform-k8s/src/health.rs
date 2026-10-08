//! Health (R-203): an object's state as its kind's `status` says it now,
//! for `dform status` and nothing else. The kinds judged are the
//! workloads and what a workload waits on: Deployment, StatefulSet,
//! DaemonSet, Job, CronJob, Service, PersistentVolumeClaim and Pod, by
//! their own name and by their short one (`k8s.deployment`). Each is
//! judged from one GET, with no clock: progressing is on its way (a
//! rollout, a pod pulling its image), degraded is failing and not getting
//! better by itself (the API gave up: `ProgressDeadlineExceeded`,
//! `BackoffLimitExceeded`; or names a failure: `CrashLoopBackOff`, a
//! claim `Lost`), suspended is stopped on purpose (`spec.paused`,
//! `spec.suspend`).
//!
//! Not judged: Ingress (its status is its controller's, and many never
//! fill it in), a ConfigMap, a Secret, RBAC, a Namespace (there is
//! nothing to be but there), and a custom resource (a kstatus-style
//! judgment of any kind with `observedGeneration` and a `Ready` condition
//! is the generalization, for later).

use crate::openapi::{self, Kind};
use dform_core::plugin::backend::health;
use dform_grpc::pb::{self, HealthState as S};
use serde_json::Value as Json;

/// The kinds judged, by group and kind.
const KINDS: [(&str, &str, &str); 8] = [
    ("apps", "v1", "Deployment"),
    ("apps", "v1", "StatefulSet"),
    ("apps", "v1", "DaemonSet"),
    ("batch", "v1", "Job"),
    ("batch", "v1", "CronJob"),
    ("", "v1", "Service"),
    ("", "v1", "PersistentVolumeClaim"),
    ("", "v1", "Pod"),
];

/// The types the provider answers Health for: each judged kind's, and
/// each short name of one.
pub fn types() -> Vec<String> {
    let own: Vec<String> = KINDS
        .iter()
        .map(|(g, v, k)| openapi::type_name(g, v, k))
        .collect();
    let short = openapi::aliases()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, t)| own.contains(t))
        .map(|(a, _)| a);
    let mut out: Vec<String> = own.iter().cloned().chain(short).collect();
    out.sort();
    out
}

/// Whether `kind` is one judged.
pub fn judged(kind: &Kind) -> bool {
    KINDS
        .iter()
        .any(|(g, _, k)| kind.group == *g && kind.kind == *k)
}

/// The object `o` of `kind`, judged; `None` (it is gone) is degraded.
pub fn judge(kind: &Kind, o: Option<&Json>) -> pb::Health {
    let Some(o) = o else {
        return health(S::Degraded, "not found");
    };
    match kind.kind.as_str() {
        "Deployment" => deployment(o),
        "StatefulSet" => stateful_set(o),
        "DaemonSet" => daemon_set(o),
        "Job" => job(o),
        "CronJob" => cron_job(o),
        "Service" => service(o),
        "PersistentVolumeClaim" => claim(o),
        "Pod" => pod(o),
        other => health(S::Unknown, format!("the provider judges no {other}")),
    }
}

fn int(o: &Json, path: &str) -> Option<i64> {
    o.pointer(path).and_then(Json::as_i64)
}

fn text<'a>(o: &'a Json, path: &str) -> Option<&'a str> {
    o.pointer(path).and_then(Json::as_str)
}

fn flag(o: &Json, path: &str) -> bool {
    o.pointer(path).and_then(Json::as_bool).unwrap_or(false)
}

/// The condition of `type` in `status.conditions`.
fn condition<'a>(o: &'a Json, typ: &str) -> Option<&'a Json> {
    o.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Json::as_str) == Some(typ))
}

/// Whether condition `typ` has status `status`.
fn is(o: &Json, typ: &str, status: &str) -> bool {
    condition(o, typ).and_then(|c| c.get("status")?.as_str()) == Some(status)
}

/// The controller has not seen the latest spec yet.
fn unobserved(o: &Json) -> Option<pb::Health> {
    let (generation, observed) = (
        int(o, "/metadata/generation")?,
        int(o, "/status/observedGeneration").unwrap_or(0),
    );
    (observed < generation).then(|| {
        health(
            S::Progressing,
            format!("the controller has not seen generation {generation} yet"),
        )
    })
}

/// A rollout's replicas: `want` of them, `updated` and `available`.
fn replicas(want: i64, updated: i64, available: i64) -> pb::Health {
    if updated < want {
        return health(S::Progressing, format!("{updated} of {want} updated"));
    }
    match available < want {
        true => health(S::Progressing, format!("{available} of {want} available")),
        false => health(S::Healthy, format!("{available} of {want} available")),
    }
}

fn deployment(o: &Json) -> pb::Health {
    let want = int(o, "/spec/replicas").unwrap_or(1);
    let available = int(o, "/status/availableReplicas").unwrap_or(0);
    if flag(o, "/spec/paused") {
        return health(
            S::Suspended,
            format!("paused, {available} of {want} available"),
        );
    }
    if let Some(c) = condition(o, "Progressing")
        && c.get("status").and_then(Json::as_str) == Some("False")
        && c.get("reason").and_then(Json::as_str) == Some("ProgressDeadlineExceeded")
    {
        return health(
            S::Degraded,
            format!("ProgressDeadlineExceeded: {available} of {want} available"),
        );
    }
    if let Some(h) = unobserved(o) {
        return h;
    }
    let updated = int(o, "/status/updatedReplicas").unwrap_or(0);
    let old = int(o, "/status/replicas").unwrap_or(0) - updated;
    if updated >= want && old > 0 {
        return health(
            S::Progressing,
            format!("{old} old replicas pending termination"),
        );
    }
    replicas(want, updated, available)
}

fn stateful_set(o: &Json) -> pb::Health {
    if let Some(h) = unobserved(o) {
        return h;
    }
    let want = int(o, "/spec/replicas").unwrap_or(1);
    let updated = match text(o, "/spec/updateStrategy/type") {
        // OnDelete: a pod is updated when someone deletes it.
        Some("OnDelete") => want,
        _ => int(o, "/status/updatedReplicas").unwrap_or(0),
    };
    let available = int(o, "/status/availableReplicas")
        .or_else(|| int(o, "/status/readyReplicas"))
        .unwrap_or(0);
    replicas(want, updated, available)
}

fn daemon_set(o: &Json) -> pb::Health {
    if let Some(h) = unobserved(o) {
        return h;
    }
    let want = int(o, "/status/desiredNumberScheduled").unwrap_or(0);
    let updated = int(o, "/status/updatedNumberScheduled").unwrap_or(0);
    let available = int(o, "/status/numberAvailable").unwrap_or(0);
    replicas(want, updated, available)
}

fn job(o: &Json) -> pb::Health {
    if is(o, "Complete", "True") {
        return health(S::Healthy, "complete");
    }
    if let Some(c) = condition(o, "Failed")
        && c.get("status").and_then(Json::as_str) == Some("True")
    {
        let reason = c.get("reason").and_then(Json::as_str).unwrap_or("failed");
        return health(S::Degraded, reason);
    }
    if flag(o, "/spec/suspend") {
        return health(S::Suspended, "suspended");
    }
    let active = int(o, "/status/active").unwrap_or(0);
    health(S::Progressing, format!("{active} active"))
}

fn cron_job(o: &Json) -> pb::Health {
    if flag(o, "/spec/suspend") {
        return health(S::Suspended, "suspended");
    }
    match text(o, "/status/lastScheduleTime") {
        Some(at) => health(S::Healthy, format!("last scheduled {at}")),
        None => health(S::Healthy, "not scheduled yet"),
    }
}

/// A Service is there once it exists, but a LoadBalancer's, which waits
/// for its address.
fn service(o: &Json) -> pb::Health {
    if text(o, "/spec/type") != Some("LoadBalancer") {
        return health(S::Healthy, "");
    }
    let ingress = o
        .pointer("/status/loadBalancer/ingress")
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let at: Vec<&str> = ingress
        .iter()
        .filter_map(|i| {
            i.get("ip")
                .or_else(|| i.get("hostname"))
                .and_then(Json::as_str)
        })
        .collect();
    match at.is_empty() {
        true => health(S::Progressing, "waiting for a load balancer address"),
        false => health(S::Healthy, at.join(", ")),
    }
}

fn claim(o: &Json) -> pb::Health {
    match text(o, "/status/phase") {
        Some("Bound") => health(S::Healthy, "Bound"),
        Some("Lost") => health(S::Degraded, "Lost: its volume is gone"),
        Some("Pending") => health(
            S::Progressing,
            "Pending: waiting for a volume, or for its first consumer",
        ),
        Some(other) => health(S::Unknown, other),
        None => health(S::Progressing, "no phase yet"),
    }
}

/// A container that waits: its name and why.
fn waiting<'a>(c: &&'a Json) -> Option<(&'a str, &'a str)> {
    let name = c.get("name").and_then(Json::as_str).unwrap_or("?");
    let reason = c.pointer("/state/waiting/reason").and_then(Json::as_str)?;
    Some((name, reason))
}

/// What a waiting container's reason names as a failure, not a step on
/// the way.
const FAILING: [&str; 6] = [
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
];

fn pod(o: &Json) -> pb::Health {
    let statuses: Vec<&Json> = ["/status/initContainerStatuses", "/status/containerStatuses"]
        .iter()
        .filter_map(|p| o.pointer(p).and_then(Json::as_array))
        .flatten()
        .collect();
    if let Some((name, reason)) = statuses
        .iter()
        .filter_map(waiting)
        .find(|(_, r)| FAILING.contains(r))
    {
        return health(S::Degraded, format!("{reason}: container {name}"));
    }
    match text(o, "/status/phase") {
        Some("Succeeded") => health(S::Healthy, "Succeeded"),
        Some("Failed") => health(
            S::Degraded,
            text(o, "/status/reason").unwrap_or("Failed").to_string(),
        ),
        Some("Running") => {
            let containers = o
                .pointer("/status/containerStatuses")
                .and_then(Json::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let ready = containers
                .iter()
                .filter(|c| c.get("ready").and_then(Json::as_bool) == Some(true))
                .count();
            match ready == containers.len() {
                true => health(S::Healthy, "Running"),
                false => health(
                    S::Progressing,
                    format!("{ready} of {} containers ready", containers.len()),
                ),
            }
        }
        Some("Pending") => match statuses.iter().filter_map(waiting).next() {
            Some((name, reason)) => health(
                S::Progressing,
                format!("Pending: {reason}: container {name}"),
            ),
            None => health(S::Progressing, "Pending"),
        },
        Some(other) => health(S::Unknown, other),
        None => health(S::Progressing, "no phase yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(group: &str, k: &str) -> Kind {
        Kind {
            group: group.into(),
            version: "v1".into(),
            kind: k.into(),
            plural: String::new(),
            namespaced: true,
        }
    }

    fn judged(group: &str, k: &str, o: Json) -> (S, String) {
        let h = judge(&kind(group, k), Some(&o));
        (S::try_from(h.state).unwrap(), h.reason)
    }

    /// The types are the snapshot's own names and their short ones; a
    /// kind it does not judge is not among them.
    #[test]
    fn the_types_answered_are_the_judged_kinds_by_both_names() {
        let t = types();
        for name in [
            "k8s.apps.v1.deployment",
            "k8s.deployment",
            "k8s.batch.v1.cron_job",
            "k8s.cron_job",
            "k8s.core.v1.pod",
            "k8s.pod",
            "k8s.persistent_volume_claim",
        ] {
            assert!(t.contains(&name.to_string()), "{name}: {t:?}");
        }
        assert!(
            !t.iter()
                .any(|n| n.contains("ingress") || n.contains("config_map"))
        );
    }

    /// Every judged kind's type is one the snapshot derives.
    #[test]
    fn every_type_answered_is_a_kind_of_the_snapshot() {
        let d = openapi::snapshot().unwrap();
        for t in types() {
            let k = d.kind(&t).unwrap_or_else(|e| panic!("{t}: {e}"));
            assert!(super::judged(k), "{t}");
        }
    }

    #[test]
    fn a_deployment_is_judged_by_its_replicas_and_its_deadline() {
        let d = |spec: Json, status: Json| {
            judged(
                "apps",
                "Deployment",
                json!({"metadata": {"generation": 2}, "spec": spec, "status": status}),
            )
        };
        let all = json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 3,
                         "availableReplicas": 3});
        assert_eq!(
            d(json!({"replicas": 3}), all.clone()),
            (S::Healthy, "3 of 3 available".into())
        );
        assert_eq!(
            d(
                json!({"replicas": 3}),
                json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 3,
                       "availableReplicas": 1})
            ),
            (S::Progressing, "1 of 3 available".into())
        );
        assert_eq!(
            d(
                json!({"replicas": 3}),
                json!({"observedGeneration": 2, "replicas": 4, "updatedReplicas": 3,
                       "availableReplicas": 3})
            ),
            (S::Progressing, "1 old replicas pending termination".into())
        );
        assert_eq!(
            d(
                json!({"replicas": 3}),
                json!({"observedGeneration": 1, "replicas": 3, "updatedReplicas": 3,
                       "availableReplicas": 3})
            ),
            (
                S::Progressing,
                "the controller has not seen generation 2 yet".into()
            )
        );
        assert_eq!(
            d(
                json!({"replicas": 3}),
                json!({"observedGeneration": 2, "availableReplicas": 1, "conditions": [
                    {"type": "Progressing", "status": "False",
                     "reason": "ProgressDeadlineExceeded"}]})
            ),
            (
                S::Degraded,
                "ProgressDeadlineExceeded: 1 of 3 available".into()
            )
        );
        assert_eq!(
            d(json!({"replicas": 3, "paused": true}), all).0,
            S::Suspended
        );
    }

    #[test]
    fn a_stateful_set_and_a_daemon_set_are_judged_by_their_replicas() {
        let s = |status: Json| {
            judged(
                "apps",
                "StatefulSet",
                json!({"metadata": {"generation": 1}, "spec": {"replicas": 2}, "status": status}),
            )
        };
        assert_eq!(
            s(json!({"observedGeneration": 1, "updatedReplicas": 1, "availableReplicas": 2})),
            (S::Progressing, "1 of 2 updated".into())
        );
        assert_eq!(
            s(json!({"observedGeneration": 1, "updatedReplicas": 2, "availableReplicas": 2})).0,
            S::Healthy
        );
        let d = |status: Json| {
            judged(
                "apps",
                "DaemonSet",
                json!({"metadata": {"generation": 1}, "status": status}),
            )
        };
        assert_eq!(
            d(json!({"observedGeneration": 1, "desiredNumberScheduled": 3,
                     "updatedNumberScheduled": 3, "numberAvailable": 2})),
            (S::Progressing, "2 of 3 available".into())
        );
        assert_eq!(
            d(json!({"observedGeneration": 1, "desiredNumberScheduled": 3,
                     "updatedNumberScheduled": 3, "numberAvailable": 3}))
            .0,
            S::Healthy
        );
    }

    #[test]
    fn a_job_is_complete_failed_suspended_or_running() {
        let j = |spec: Json, status: Json| {
            judged("batch", "Job", json!({"spec": spec, "status": status}))
        };
        assert_eq!(
            j(
                json!({}),
                json!({"conditions": [{"type": "Complete", "status": "True"}]})
            ),
            (S::Healthy, "complete".into())
        );
        assert_eq!(
            j(
                json!({}),
                json!({"conditions": [{"type": "Failed", "status": "True",
                                       "reason": "BackoffLimitExceeded"}]})
            ),
            (S::Degraded, "BackoffLimitExceeded".into())
        );
        assert_eq!(j(json!({"suspend": true}), json!({})).0, S::Suspended);
        assert_eq!(
            j(json!({}), json!({"active": 1})),
            (S::Progressing, "1 active".into())
        );
        let c = |spec: Json| judged("batch", "CronJob", json!({"spec": spec, "status": {}}));
        assert_eq!(c(json!({"suspend": true})).0, S::Suspended);
        assert_eq!(c(json!({})).0, S::Healthy);
    }

    #[test]
    fn a_load_balancer_waits_for_its_address_and_a_claim_for_its_volume() {
        let svc = |spec: Json, status: Json| {
            judged("", "Service", json!({"spec": spec, "status": status}))
        };
        assert_eq!(
            svc(json!({"type": "LoadBalancer"}), json!({"loadBalancer": {}})).0,
            S::Progressing
        );
        assert_eq!(
            svc(
                json!({"type": "LoadBalancer"}),
                json!({"loadBalancer": {"ingress": [{"ip": "203.0.113.7"}]}})
            ),
            (S::Healthy, "203.0.113.7".into())
        );
        assert_eq!(svc(json!({"type": "ClusterIP"}), json!({})).0, S::Healthy);
        let pvc = |phase: &str| {
            judged(
                "",
                "PersistentVolumeClaim",
                json!({"status": {"phase": phase}}),
            )
            .0
        };
        assert_eq!(
            [pvc("Bound"), pvc("Pending"), pvc("Lost")],
            [S::Healthy, S::Progressing, S::Degraded]
        );
    }

    #[test]
    fn a_crash_looping_pod_is_degraded_with_its_reason() {
        let p = |status: Json| judged("", "Pod", json!({"status": status}));
        assert_eq!(
            p(json!({"phase": "Running", "containerStatuses": [
                {"name": "web", "ready": false,
                 "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]})),
            (S::Degraded, "CrashLoopBackOff: container web".into())
        );
        assert_eq!(
            p(json!({"phase": "Running", "containerStatuses": [
                {"name": "web", "ready": true, "state": {"running": {}}}]})),
            (S::Healthy, "Running".into())
        );
        assert_eq!(
            p(json!({"phase": "Pending", "containerStatuses": [
                {"name": "web", "ready": false,
                 "state": {"waiting": {"reason": "ContainerCreating"}}}]})),
            (
                S::Progressing,
                "Pending: ContainerCreating: container web".into()
            )
        );
        assert_eq!(p(json!({"phase": "Succeeded"})).0, S::Healthy);
        assert_eq!(
            p(json!({"phase": "Failed", "reason": "Evicted"})).1,
            "Evicted"
        );
    }

    #[test]
    fn an_object_that_is_gone_is_degraded() {
        let h = judge(&kind("apps", "Deployment"), None);
        assert_eq!(
            (S::try_from(h.state).unwrap(), h.reason.as_str()),
            (S::Degraded, "not found")
        );
    }
}
