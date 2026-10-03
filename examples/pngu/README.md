# pngu
A GKE stack modelled on a Terraform one, a deployment per env, its peerings a CSV table.
```bash
dform plan                 # pngu[env=dev]: 7 creates
dform apply pngu env=dev   # keyed: apply names the deployment; one tick
dform plan pngu env=prod   # its own state and GCP project: creates
```
The provider (the gke mock, playing the google types) is configured by the key and refuses
another env's account. Features: a keyed stack, a type alias, `decl p(cols)` and `input p from csv`, the loader `json(path)`, `expect_account`.
