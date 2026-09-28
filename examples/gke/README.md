# gke
Two-phase GKE: the cluster's zones, endpoint and CA are open nulls until it exists. Two stacks: name one.
```bash
dform plan gke_two_phase
dform apply gke_two_phase   # 2 ticks
dform plan gke_one_zone
dform apply gke_one_zone    # stops after tick 1, on purpose
```
gke_one_zone's cluster comes back in one zone and its `undetermined:` deny stops apply.
Features: pending groups over null lists, `provider_config`, aggregates.
