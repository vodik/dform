# gke
Two-phase GKE: the cluster's zones, endpoint and CA are open nulls until it exists.
```bash
dform plan
dform apply                 # 2 ticks: the cluster; its node pools and the kubernetes objects
dform apply --set zones=1   # stops after tick 1, on purpose: one zone is fewer than the deny allows
```
The plan's `later` holds the two-zones deny, `until tick 2`: decided once the cluster's zones are known.
Features: pending groups over null lists, a `use k8s { .. }` block configured from open nulls and a data source's secret, aggregates, typed inputs.
