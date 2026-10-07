| Variable | Meaning |
|---|---|
| `DFORM_LOG` | `debug`: a line on stderr for each phase of a run and each call it makes over the network, with its wall time |
| `DFORM_ACTOR` | who acts, as the audit log records it (a CI job's OIDC subject); else `USER@HOST`, from `USER` or `LOGNAME` |
| `DFORM_EXPERIMENTAL` | `1` lists the experimental commands (`controller`, `stack handover`) in `--help` and the completions; they run either way |
| `DFORM_CREDENTIALS` | the directory of the operator's credential files, instead of `$XDG_CONFIG_HOME/dform/credentials` |
| `DFORM_S3_ACCESS_KEY_ID`, `DFORM_S3_SECRET_ACCESS_KEY`, `DFORM_S3_SESSION_TOKEN` | the s3 state backend's credentials; else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`; only the environment, no profile file |
| `DFORM_PROVIDER_TRANSPORT` | `unix`: a provider listens on a socket in the temporary directory instead of TCP on the loopback interface (providers inherit dform's environment) |
| `DFORM_PROVIDER_FAKE` | an executable to run as the mock provider instead of dform's own |
| `DFORM_K8S_OFFLINE` | set: the Kubernetes provider is offline, its schema the snapshot built into it; Read, Apply and Import fail naming why |
| `KUBECONFIG` | the cluster the Kubernetes provider applies to; else `~/.kube/config`, else the pod's service account |
| `OVH_ENDPOINT`, `OVH_APPLICATION_KEY`, `OVH_APPLICATION_SECRET`, `OVH_CONSUMER_KEY` | the OVH provider's credentials, over `ovh.conf`'s |
| `DFORM_OVH_RESOLVER` | `IP:PORT`: the DNS server the OVH provider asks a zone's nameservers of, instead of the first `nameserver` of `/etc/resolv.conf` |
| `SSH_AUTH_SOCK` | the agent whose keys the `ssh` provider offers first |
| `NO_COLOR` | set and not empty: `--color=auto` colours nothing |
| `XDG_CONFIG_HOME` | where the credential files (`dform/credentials/`) and OVH's `ovh/ovh.conf` are; default `~/.config` |
| `XDG_CACHE_HOME` | where git mirrors, compiled wasm providers and the language server's read-only files are kept (`dform/`); default `~/.cache` |
| `MANPAGER`, `PAGER` | the pager `dform help` runs on a terminal (`sh -c`), `MANPAGER` first; with neither the page is printed |
