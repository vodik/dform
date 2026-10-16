# k3s-platform

A small team's Kubernetes on OVH Public Cloud: one instance with k3s on
it, the cluster's API under the team's domain, and the web app in the
cluster with its Service and its name, in staging and in prod. Two
stacks: `platform` is the instance and the cluster, `apps` what runs in
it, configured from the platform's kubeconfig. `policy.df` is the team's
policy: k3s gets a server with 4 GB of memory, and every image runs as
the digest the plan saw. The providers are the mock playing OVH's types
(`providers/ovh`) and Kubernetes' (dform.toml).

From this directory, `dform` being `cargo run --` in a checkout:

```
dform plan                         # platform then apps, staging and prod
dform plan platform                # the instance, then what waits on it
dform render apps env=staging      # the apps' manifests, for Argo CD or kustomize
```

Applying needs a server that answers SSH: the kubeconfig is read from
it once it is up, and the mock's instance is no host.

## One plan, three layers

The instance, k3s on it and what runs in the cluster are one program:
the Kubernetes provider is configured from the server's kubeconfig, read
over SSH, so everything it serves waits on it, in the tick after the
server's:

<!-- run in examples/k3s-platform -->
```
$ dform plan platform
deployment: stacks.platform[env=staging]
plan: 4 changes (4 create) over 2 ticks; policy: 3 hold

tick 1  2 changes
  + ovh.ssh_key admin          stacks/platform.df:18
      name = "k3s-staging-admin"
      public_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NT…q3m7cJ4yQx1aB ops@example.com"
  + ovh.instance server        stacks/platform.df:25
      flavor = "b2-7"          stacks/platform.df:4
      image = "Ubuntu 24.04"
      name = "k3s-staging"
      region = "BHS5"
      ssh_key = admin
      user_data = (sensitive)  stacks/platform.df:23

tick 2  2 changes
  waits on  server.public_ip
            provider k8s  kubeconfig
  + ovh.domain_record api      stacks/platform.df:42
      subdomain = "api.staging"
      target = server.public_ip
      ttl = 300
      type = "A"
      zone = "example.com"
  + k8s.namespace apps         stacks/platform.df:39
      metadata.name = "apps"

policy  3 hold
```

The apps stack reads the platform's outputs, `platform[env].kubeconfig`,
so `dform apply apps env=prod` applies the platform first.

## The pipeline

`render` prints what apply would send to the cluster, policy checked,
with no credentials and no state:

<!-- run in examples/k3s-platform -->
```
$ dform render apps env=staging
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: apps
spec:
  replicas: 2
...
```
