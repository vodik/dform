# adopt
Adopting an existing resource from inventory instead of creating it.
```bash
dform plan    # staging: one create
dform apply   # one tick
dform dev --inventory ../../tests/fixtures/world/inventory.json plan --set env=prod
```
The prod plan adopts the inventory's `existing-prod-vpc` (`>`).
Features: `adopt`, `cloud_ref`, `world.T`, a `set` per environment giving a used module's input (`set network.vpc_net = .. where ..`).
