//! Kubernetes CustomResourceDefinitions (R-126): the types a CRD the
//! program makes defines, by the names the Kubernetes provider serves
//! them under (`crates/dform-k8s/src/openapi.rs` derives them with these
//! functions), so a resource of such a type waits on the CRD that makes
//! it rather than failing as a kind the cluster does not serve.
//!
//! A kind is the type `k8s.<group>.<version>.<kind>` (the core group as
//! `core`, `-` in the group as `_`, the kind in snake_case) and, for a
//! cluster's own kind (a group the static schema does not have), also
//! `k8s.<the group's first label>.<kind>` at its preferred version:
//! `traefik.io` + `Middleware` is `k8s.traefik.io.v1alpha1.middleware`
//! and `k8s.traefik.middleware`.

use crate::value::Value;
use std::cmp::Reverse;

/// The CustomResourceDefinition type, by its short name and its full one.
pub const TYPES: [&str; 2] = [
    "k8s.custom_resource_definition",
    "k8s.apiextensions.k8s.io.v1.custom_resource_definition",
];

/// `k8s.apps.v1.deployment`, `k8s.core.v1.config_map`.
pub fn type_name(group: &str, version: &str, kind: &str) -> String {
    let group = if group.is_empty() { "core" } else { group };
    format!(
        "k8s.{}.{}.{}",
        group.replace('-', "_"),
        version,
        snake(kind)
    )
}

/// A cluster's own kind under its group's first label:
/// `k8s.traefik.middleware`, `k8s.cert_manager.certificate`.
pub fn short_name(group: &str, kind: &str) -> String {
    let label = group.split('.').next().unwrap_or(group);
    format!("k8s.{}.{}", label.replace('-', "_"), snake(kind))
}

/// `HorizontalPodAutoscaler` -> `horizontal_pod_autoscaler`,
/// `CSIDriver` -> `csi_driver`.
pub fn snake(kind: &str) -> String {
    let cs: Vec<char> = kind.chars().collect();
    let mut out = String::new();
    for (i, c) in cs.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = cs[i - 1];
            let next_lower = cs.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Kubernetes's order of versions, most preferred first: GA before beta
/// before alpha, a higher number first (`v2`, `v1`, `v1beta2`,
/// `v1beta1`, `v1alpha1`), anything else after, by name.
fn priority(v: &str) -> (u8, Reverse<u64>, Reverse<u64>, String) {
    let parse = || -> Option<(u8, u64, u64)> {
        let rest = v.strip_prefix('v')?;
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let major: u64 = rest[..end].parse().ok()?;
        match &rest[end..] {
            "" => Some((0, major, 0)),
            s => {
                let (class, n) = match (s.strip_prefix("beta"), s.strip_prefix("alpha")) {
                    (Some(n), _) => (1, n),
                    (_, Some(n)) => (2, n),
                    _ => return None,
                };
                Some((class, major, n.parse().ok()?))
            }
        }
    };
    match parse() {
        Some((class, major, minor)) => (class, Reverse(major), Reverse(minor), String::new()),
        None => (3, Reverse(0), Reverse(0), v.to_string()),
    }
}

/// The most preferred of `versions` ([`priority`]).
pub fn preferred<'a>(versions: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    versions.into_iter().min_by_key(|v| priority(v))
}

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| match v {
        Value::Obj(m) => m.get(*k),
        _ => None,
    })
}

/// The types a CRD document (its attributes) defines: each served
/// version's, and the short name of the preferred one. `None` when it
/// does not say its group and kind.
pub fn defines(doc: &Value) -> Option<Vec<String>> {
    let s = |p: &[&str]| at(doc, p).and_then(Value::as_str);
    let group = s(&["spec", "group"])?;
    let kind = s(&["spec", "names", "kind"])?;
    let versions: Vec<&str> = match at(doc, &["spec", "versions"]) {
        Some(Value::List(vs)) => vs
            .iter()
            .filter(|v| !matches!(at(v, &["served"]), Some(Value::Bool(false))))
            .filter_map(|v| at(v, &["name"]).and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = versions.iter().map(|v| type_name(group, v, kind)).collect();
    out.push(short_name(group, kind));
    Some(out)
}

/// What a type no schema has would need, for the error that nothing
/// makes it: the kind as the type names it (`traefik.middleware`) and the
/// name of the CRD that would define it, its plural as the convention
/// makes it (`middlewares.traefik.*` from a short name, whose group the
/// name gives only the first label of). `None` for a type that is no
/// Kubernetes kind's.
pub fn expected(typ: &str) -> Option<(String, String)> {
    let rest = typ.strip_prefix("k8s.")?;
    let segs: Vec<&str> = rest.split('.').collect();
    let (kind, group) = match segs.as_slice() {
        [label, kind] => (*kind, format!("{}.*", label.replace('_', "-"))),
        [group @ .., version, kind] if group.len() >= 2 && priority(version).0 < 3 => {
            (*kind, group.join(".").replace('_', "-"))
        }
        _ => return None,
    };
    Some((
        rest.to_string(),
        format!("{}s.{group}", kind.replace('_', "")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crd_defines_its_versions_and_a_short_name() {
        let doc = crate::tables::document(
            "yaml",
            "spec:\n  group: traefik.io\n  names:\n    kind: Middleware\n    plural: middlewares\n  \
             versions:\n  - name: v1alpha1\n    served: true\n  - name: v0\n    served: false\n",
        )
        .unwrap();
        assert_eq!(
            defines(&doc).unwrap(),
            vec![
                "k8s.traefik.io.v1alpha1.middleware".to_string(),
                "k8s.traefik.middleware".to_string()
            ]
        );
        assert_eq!(snake("TLSOption"), "tls_option");
        assert_eq!(
            short_name("cert-manager.io", "ClusterIssuer"),
            "k8s.cert_manager.cluster_issuer"
        );
    }

    #[test]
    fn versions_in_kubernetes_order() {
        assert_eq!(
            preferred(["v1alpha1", "v1beta2", "v1", "v1beta1", "foo"]),
            Some("v1")
        );
        assert_eq!(preferred(["v1alpha1", "v2alpha1"]), Some("v2alpha1"));
        assert_eq!(preferred(["v1beta1", "v1alpha3"]), Some("v1beta1"));
    }

    #[test]
    fn what_a_kind_needs() {
        assert_eq!(
            expected("k8s.traefik.middleware"),
            Some(("traefik.middleware".into(), "middlewares.traefik.*".into()))
        );
        assert_eq!(
            expected("k8s.traefik.io.v1alpha1.ingress_route"),
            Some((
                "traefik.io.v1alpha1.ingress_route".into(),
                "ingressroutes.traefik.io".into()
            ))
        );
        assert_eq!(expected("net.vpc"), None);
    }
}
