| Variable | Meaning |
|---|---|
| `DFORM_LOG` | `debug`: a line on stderr for each phase of a run and each call it makes over the network, with its wall time |
| `DFORM_ACTOR` | who acts, as the audit log records it (a CI job's OIDC subject); else `USER@HOST`, from `USER` or `LOGNAME` |
| `DFORM_CREDENTIALS` | the directory of the operator's credential files, instead of `$XDG_CONFIG_HOME/dform/credentials` |
| `DFORM_S3_ACCESS_KEY_ID`, `DFORM_S3_SECRET_ACCESS_KEY`, `DFORM_S3_SESSION_TOKEN` | the s3 state backend's credentials; else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`; only the environment, no profile file |
| `NO_COLOR` | set and not empty: `--color=auto` colours nothing |
| `RANDOM_MASTER` | the `random.*` input key material instead of the deployment's master (tests, the editor): taken only where state was applied with it, or nothing was applied (see "Secrets") |
| `XDG_CONFIG_HOME` | where the credential files (`dform/credentials/`) are; default `~/.config` |
| `XDG_CACHE_HOME` | where git mirrors, compiled wasm providers and the language server's read-only files are kept (`dform/`); default `~/.cache` |
