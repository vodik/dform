# crud-api
A GitOps blue/green rollout with a gated schema migration (proposal G).
```bash
dform plan    # tick 1 definite; the app is pending on the Job
dform apply   # 3 ticks: database and Job; blue; the Service's selector
dform controller run crud_api
```
The denies under the plan's `later` (`until tick 2`) are decided once the Job reports.
Features: a component with conditional resources of it, `world.T`, a derived password (`random.password`), secrets, denies.
