# crud-api

An API team's blue/green rollout on Kubernetes: CI writes the release,
the image by its digest and the database schema it needs, to
`release.toml`; the schema migration runs as a Job, the new colour's
Deployment follows once the Job has succeeded, and the Service moves to
it once every replica of it is ready. The colour that serves is the one
the live Service selects. `policy.df` is the team's policy: every
container runs an image pinned by its digest, with limits. The provider
is the Kubernetes mock (dform.toml).

From this directory, `dform` being `cargo run --` in a checkout:

```
dform plan                    # the Job; the rest waits on it
dform apply                   # three ticks: the Job, blue, the Service
dform status api              # the pipeline's check: is everything healthy
```

## The rollout

The first plan makes the migration's Job; everything after it waits on
the Job's success, and the plan says so:

<!-- run in examples/crud-api -->
```
$ dform plan
plan: 2 changes (2 create) over 2 ticks; policy: 2 undetermined

tick 1  2 changes
  + k8s.namespace shop   stacks/api.df:29
      metadata.name = "shop"
  + k8s.job migrate-v42  stacks/api.df:32
...
tick 2  ? changes
  waits on  migrate-v42.status.succeeded
  k8s.service api        stacks/api.df:21  if serving(_)
...
```

`dform apply` runs it in three ticks: the Job, then blue once the Job has
succeeded, then the Service once blue is ready.

## In a pipeline

`status` asks the cluster how each object is doing, and fails unless
everything is healthy:

<!-- run in examples/crud-api after dform apply; k8s.deployment blue.deploy is degraded: CrashLoopBackOff: container api -->
```
$ dform status api
k8s.deployment blue.deploy  degraded  CrashLoopBackOff: container api
k8s.job migrate-v42         -
k8s.namespace shop          -
k8s.service api             -
status: 1 degraded, 3 without health
```
