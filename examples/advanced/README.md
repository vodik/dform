# advanced
Transitive closure over infrastructure: reachability, routes, nested group membership.
```bash
dform plan    # 13 creates; warns for admins by group nesting
dform apply   # one tick
dform query reachable
dform query in_group
```
Features: recursive rules, resources `where` a derived relation holds, `deny`, `warn`, `query`.
