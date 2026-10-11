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
