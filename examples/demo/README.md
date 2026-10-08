# demo
The demo: modules used by their paths, a component copied twice, policy
packs, per-environment config, a keyed stack, its environments listed
in project.df.
```bash
dform plan                      # project.df's three deployments: their states, then each plan
dform apply                     # each of them, in turn
dform apply dform env=staging   # keyed: apply names the deployment; one tick
dform plan dform env=prod       # its own deployment: creates
dform test dform                # the denies, in every env
dform test                      # each deployment project.df lists
dform dev effects               # per scope: what it reads, writes, offers
```
Features: `project.df` (the environments as code: a deployment per env, R-114), `key env`, `set from` a document per env, dform.toml's `[stacks.dform]` (`isolated`),
`use database { .. }` (a module with inputs and resources), `resource
network.vpc main` (a resource of a component, network.df's `vpc`), `use baseline` (a
policy pack), `network.subnets` (another module's type alias), denies
`dform test` runs over every env.
