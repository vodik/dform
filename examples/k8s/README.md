# k8s
A Namespace, a Deployment, a Service and a ConfigMap on the mock Kubernetes provider.
```bash
dform plan    # 4 creates
dform apply   # one tick
```
The ConfigMap uses `generateName`: the Deployment's `configMapRef` is a null until Apply.
Features: the k8s schema, keyed lists (`containers[name=web]`), open nulls.
