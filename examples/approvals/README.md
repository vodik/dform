# approvals
Policy says which changes need an approval; apply checks a signed approval of the plan's digest.
```bash
dform plan
dform apply approvals env=staging   # keyed: apply names the deployment; one tick
```
Staging needs no approval. A replace in prod does: the signing flow is in docs/reference.md ("Approvals").
Features: a keyed stack, dform.toml's `approvals = 'jwks_file(...)'`, `requires_approval`.
