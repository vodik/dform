# demo
The demo: modules used by their paths, a component copied twice, policy
packs, per-environment config, a keyed stack.
```bash
dform plan                      # dform[env=staging]: 13 creates
dform apply dform env=staging   # keyed: apply names the deployment; one tick
dform plan dform env=prod       # its own deployment: creates
dform test dform                # the denies, in every env
dform dev effects               # per scope: what it reads, writes, offers
```
Features: `key env`, `set from` a document per env, dform.toml's `[stacks.dform]` (`isolated`),
`use database { .. }` (a module with inputs and resources), `resource
network.vpc main` (a resource of a component, network.df's `vpc`), `use baseline` (a
policy pack), `network.subnets` (another module's type alias), denies
`dform test` runs over every env.
