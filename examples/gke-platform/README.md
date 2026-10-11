# gke-platform
A GKE platform modelled on a Terraform stack: per env (dev, stg, prod), its own GCP project holding the VPC network and its peerings (a CSV table), the GKE and proxy subnets, an internal static IP, a private Autopilot cluster, a monitoring dashboard and an alert pack. Each env is a deployment with its own state.
```bash
dform plan                         # gke_platform[env=dev]: 7 creates
dform apply gke_platform env=dev   # keyed: apply names the deployment; one tick
dform plan gke_platform env=prod   # its own state and GCP project: creates
```
The provider (the gke mock, playing the google types) is configured by the key and refuses
another env's account. Features: a keyed stack, a type alias, `decl p(cols)` and `input p from csv.decode(io.read(..))`, the read `json.decode(io.read(path))`, `expect_account`.
