# crud-api
A GitOps blue/green rollout with a gated schema migration (proposal G).
```bash
dform plan    # tick 1 definite; the app is pending on the Job
dform apply   # 3 ticks: database and Job; blue; the Service's selector
dform controller run crud_api
```
The plan's `undetermined:` denies are decided once the Job reports.
Features: a component with conditional instances, `world.T`, a derived password (`random.password`), secrets, denies.
