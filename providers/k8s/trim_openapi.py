#!/usr/bin/env python3
"""Build providers/k8s/openapi-snapshot.json: the schema the Kubernetes
provider derives from when no cluster is reachable.

Input: a directory of the API server's OpenAPI v3 documents as the
Kubernetes repository checks them in (api/openapi-spec/v3/*_openapi.json,
one per group-version). Output: the shape the provider caches a cluster's
`/openapi/v3` in, `{"paths": {"apis/apps/v1": DOC, ...}}`, trimmed to the
kinds below: each document keeps the `{name}` path of each kind (its patch
operation's group-version-kind only) and the component schemas those kinds
reach, without descriptions or defaults.

    python3 providers/k8s/trim_openapi.py DIR v1.36.0 > providers/k8s/openapi-snapshot.json
"""

import json
import os
import sys

KINDS = {
    "api/v1": ["Namespace", "ConfigMap", "Secret", "ServiceAccount", "Service",
               "PersistentVolumeClaim", "Pod"],
    "apis/apps/v1": ["Deployment", "StatefulSet", "DaemonSet", "ReplicaSet"],
    "apis/batch/v1": ["Job", "CronJob"],
    "apis/autoscaling/v2": ["HorizontalPodAutoscaler"],
    "apis/policy/v1": ["PodDisruptionBudget"],
    "apis/networking.k8s.io/v1": ["NetworkPolicy", "Ingress"],
    "apis/rbac.authorization.k8s.io/v1": ["Role", "RoleBinding", "ClusterRole",
                                          "ClusterRoleBinding"],
}

DROP = {"description", "default", "uniqueItems", "x-kubernetes-patch-strategy",
        "x-kubernetes-patch-merge-key"}


def strip(v):
    if isinstance(v, dict):
        return {k: strip(x) for k, x in v.items() if k not in DROP}
    if isinstance(v, list):
        return [strip(x) for x in v]
    return v


def refs(v, out):
    if isinstance(v, dict):
        r = v.get("$ref")
        if isinstance(r, str):
            out.add(r.rsplit("/", 1)[-1])
        for x in v.values():
            refs(x, out)
    elif isinstance(v, list):
        for x in v:
            refs(x, out)


def trim(doc, kinds):
    paths = {}
    roots = set()
    for path, item in doc["paths"].items():
        patch = item.get("patch") or {}
        gvk = patch.get("x-kubernetes-group-version-kind")
        if not path.endswith("{name}") or not gvk or gvk["kind"] not in kinds:
            continue
        paths[path] = {"patch": {
            "x-kubernetes-action": patch.get("x-kubernetes-action"),
            "x-kubernetes-group-version-kind": gvk,
        }}
    schemas = doc["components"]["schemas"]
    for name, s in schemas.items():
        for gvk in s.get("x-kubernetes-group-version-kind", []):
            if gvk["kind"] in kinds:
                roots.add(name)
    keep, todo = set(), list(roots)
    while todo:
        n = todo.pop()
        if n in keep:
            continue
        keep.add(n)
        found = set()
        refs(schemas[n], found)
        todo.extend(found - keep)
    return {"paths": paths,
            "components": {"schemas": {n: strip(schemas[n]) for n in sorted(keep)}}}


def main():
    src, version = sys.argv[1], sys.argv[2]
    out = {"x-dform-source": f"Kubernetes {version} api/openapi-spec/v3, trimmed by "
                             "providers/k8s/trim_openapi.py",
           "paths": {}}
    for gv, kinds in KINDS.items():
        with open(os.path.join(src, gv.replace("/", "__") + "_openapi.json")) as f:
            out["paths"][gv] = trim(json.load(f), set(kinds))
    json.dump(out, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
