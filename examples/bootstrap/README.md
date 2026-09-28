# bootstrap
Two stacks (name one): a cluster with dform installed as its controller, and the workload it runs.
```bash
dform plan renfry.bootstrap
dform apply renfry.bootstrap   # 3 ticks: cluster; node pools and namespace; controller
dform plan renfry.workload
dform apply renfry.workload    # one tick; or hand it over and run it as the controller:
dform stack handover renfry.workload --to 'k8s("dform-system/workload")'
```
Then `dform controller run renfry.workload`. Features: `provider_config` from open nulls, pending groups, input relations, handover.
