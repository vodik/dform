# refine
Refinement types: the cluster's zones must number at least three, checked once they are known.
```bash
dform plan                  # the refinement on the zones is deferred
dform apply                 # 2 ticks
dform apply --set zones=2   # stops after tick 1: refinement violated, on purpose
```
Features: `type T { f: ... where ... }` on a resource and on settings, typed inputs with `where`.
