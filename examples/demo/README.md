# demo
The demo: modules, a policy pack, per-environment config, a keyed stack.
```bash
dform plan                      # dform[env=staging]: 14 creates
dform apply dform env=staging   # keyed: apply names the deployment; one tick
dform plan dform env=prod       # its own deployment: creates
dform test dform                # the denies, in every env
dform dev effects               # per scope: what it reads, writes, offers
```
Features: `key env`, dform.toml's `[stacks.dform]` (`config`, `isolated`), modules and instances, a policy pack,
a type alias, denies `dform test` runs over every env.
