# approvals
Policy says which deformations need an approval; apply checks a signed approval of the plan's digest.
```bash
dform plan
dform apply approvals.demo env=staging   # keyed: apply names the deployment; one tick
```
Staging needs no approval. A replace in prod does: the signing flow is in the top-level README ("Approvals").
Features: a keyed stack, `approvals = jwks_file(...)`, `requires_approval`.
