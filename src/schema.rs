//! Hand-written provider schemas for the F-revision prototype.
//!
//! Proposal E says the null class of `ref(T, A, Attr)` comes from the schema
//! (§2.2). Nothing in the repo carries such a schema (the fake provider's
//! `catalog()` only emits type_provider/capability/tag_path), so this module
//! writes the two schemas the experiments need by hand:
//!
//!   * `fake()`: the fake provider behind dform.df / dform-advanced.df /
//!     the examples, with the computed attributes `computed_for` in
//!     fakecloud.rs actually returns.
//!   * `gke()`: C's two-phase GKE / kubernetes stack as re-run in E §7.4.
//!
//! Both are used by the partition pass (to expand the computed-attribute
//! prelude rule per schema row, E §4.3) and by the stuck simulation (to
//! classify refs).

use crate::value::NullClass;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// (type, attr) -> null class, for every computed attribute.
    pub computed: BTreeMap<(String, String), NullClass>,
    /// type -> provider name. Used for "provider config carries a null"
    /// phase assignment.
    pub provider_of: BTreeMap<String, String>,
}

impl Schema {
    pub fn class_of(&self, typ: &str, attr: &str) -> Option<NullClass> {
        self.computed.get(&(typ.to_string(), attr.to_string())).copied()
    }
    pub fn computed_of(&self, typ: &str) -> Vec<(String, NullClass)> {
        self.computed
            .iter()
            .filter(|((t, _), _)| t == typ)
            .map(|((_, a), c)| (a.clone(), *c))
            .collect()
    }
    pub fn types(&self) -> Vec<String> {
        let mut v: Vec<String> = self.computed.keys().map(|(t, _)| t.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
    fn add(&mut self, typ: &str, attr: &str, class: NullClass) {
        self.computed.insert((typ.into(), attr.into()), class);
    }
}

/// The fake provider. `computed_for` in fakecloud.rs returns `id` for every
/// type, plus `endpoint` for db.postgres and `api_endpoint` / `ca_cert` for
/// k8s.cluster. `id` is fresh (an identity); the others are open.
pub fn fake() -> Schema {
    let mut s = Schema::default();
    for t in [
        "net.vpc",
        "net.subnet",
        "net.vpc_peering",
        "net.route",
        "compute.vm",
        "db.postgres",
        "k8s.cluster",
        "k8s.nodepool",
        "iam.role",
        "iam.policy",
        "iam.role_policy_attachment",
    ] {
        s.add(t, "id", NullClass::Fresh);
        s.provider_of.insert(t.into(), "fakecloud".into());
    }
    s.add("db.postgres", "endpoint", NullClass::Open);
    s.add("k8s.cluster", "api_endpoint", NullClass::Open);
    s.add("k8s.cluster", "ca_cert", NullClass::Open);
    s
}

/// C's GKE example as E §7.4 spells it: the cluster's endpoint, ca and zones
/// are open; ids are fresh; the client token and the secret version's data are
/// secret. k8s.* types belong to the `kubernetes` provider, whose
/// configuration carries three of those nulls.
pub fn gke() -> Schema {
    let mut s = Schema::default();
    for t in [
        "google_compute_subnetwork",
        "google_compute_network_peering",
        "google_compute_address",
        "google_monitoring_dashboard",
        "gke_cluster",
        "gke_nodepool",
    ] {
        s.add(t, "id", NullClass::Fresh);
        s.provider_of.insert(t.into(), "google".into());
    }
    for t in ["k8s.namespace", "k8s.deployment", "k8s.secret"] {
        s.add(t, "id", NullClass::Fresh);
        s.add(t, "uid", NullClass::Fresh);
        s.provider_of.insert(t.into(), "kubernetes".into());
    }
    s.add("gke_cluster", "endpoint", NullClass::Open);
    s.add("gke_cluster", "ca_certificate", NullClass::Open);
    s.add("gke_cluster", "zones", NullClass::Open);
    s.add("google.client_config", "access_token", NullClass::Secret);
    s.add("google.secret_manager_secret_version", "secret_data", NullClass::Secret);
    s
}
