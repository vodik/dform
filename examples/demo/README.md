# demo
The demo: modules, a policy pack, per-environment config, a keyed stack.
```bash
dform plan                      # dform[env=staging]: 14 creates
dform apply dform env=staging   # keyed: apply names the deployment; one tick
dform plan dform env=prod       # its own deployment: creates
```
Features: `stack dform[env]`, `config = yaml(...)`, modules and instances, a policy pack with grants,
a type alias, scenarios (`dform test`).
