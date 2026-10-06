#!/usr/bin/env python3
"""Build crates/dform-k8s/openapi-snapshot.json: the schema the Kubernetes
provider derives from when no cluster is reachable, its static schema
(R-110): every kind of the stable API groups. A cluster's own kinds (its
CRDs) extend it at Configure.

Input: a directory of the API server's OpenAPI v3 documents as the
Kubernetes repository checks them in (api/openapi-spec/v3/*_openapi.json,
one per group-version). Output: the shape the provider caches a cluster's
`/openapi/v3` in, `{"paths": {"apis/apps/v1": DOC, ...}}`, trimmed to the
kinds below: each document keeps the `{name}` path of each kind (its patch
operation's group-version-kind only) and the component schemas those kinds
reach. A property's `default` stays: the provider derives a defaulted leaf
as optional_computed from it; so does its `description`: the provider's
`type_doc` for the path (the language server's hover).

The documents of a release are at
https://github.com/kubernetes/kubernetes/tree/vX.Y.Z/api/openapi-spec/v3.

    python3 crates/dform-k8s/trim_openapi.py DIR v1.36.0 > crates/dform-k8s/openapi-snapshot.json
"""

import json
import os
import sys

# The stable group-versions, every kind of each (`None`): a kind is one
# with a patchable `{name}` path.
KINDS = {
    "api/v1": None,
    "apis/apps/v1": None,
    "apis/batch/v1": None,
    "apis/autoscaling/v2": None,
    "apis/policy/v1": None,
    "apis/networking.k8s.io/v1": None,
    "apis/rbac.authorization.k8s.io/v1": None,
    "apis/storage.k8s.io/v1": None,
    "apis/scheduling.k8s.io/v1": None,
    "apis/coordination.k8s.io/v1": None,
    "apis/discovery.k8s.io/v1": None,
    "apis/node.k8s.io/v1": None,
    "apis/admissionregistration.k8s.io/v1": None,
    "apis/apiextensions.k8s.io/v1": None,
    "apis/certificates.k8s.io/v1": None,
}

DROP = {"uniqueItems", "x-kubernetes-patch-strategy",
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
    if kinds is None:
        kinds = set()
        for path, item in doc["paths"].items():
            gvk = (item.get("patch") or {}).get("x-kubernetes-group-version-kind")
            if path.endswith("{name}") and gvk:
                kinds.add(gvk["kind"])
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
                             "crates/dform-k8s/trim_openapi.py",
           "paths": {}}
    for gv, kinds in KINDS.items():
        with open(os.path.join(src, gv.replace("/", "__") + "_openapi.json")) as f:
            out["paths"][gv] = trim(json.load(f), None if kinds is None else set(kinds))
    json.dump(out, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
