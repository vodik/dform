| File | |
|---|---|
| `dform.toml` | the project root: the nearest directory up from the working directory (or `-C DIR`) holding one; its providers, stacks and defaults |
| `stacks/STACK.df` | a stack; every other `.df` file is a module, named by its path from the root |
| `dform.state/` | at the project root, gitignored: per deployment its state, its master (sealed, or a key file) and audit log, and the project's registry and cache |
| `dform.state/STACK/state.json` | a deployment's state, STACK the stack's full name (`dform.state/stacks.shop/`; `dform.state/STACK/K=V/` for a keyed stack's), with `state.master` (its master sealed under `[secrets] passphrase` and to its `recipients`) or `state.key` (its master in the clear, without `[secrets]`), `state.lock` (the apply lock), `state.audit.jsonl` (the audit log) and `outputs.json` (the published outputs) beside it |
| `dform.state/stacks.json` | the registry: where each applied deployment's objects are (a directory, or `s3://..`), by its full name (`stacks.shop[env=prod]`), for the stacks that read its outputs |
| `dform.state/cache/` | what providers and trust roots fetch (the Kubernetes OpenAPI document, JWKS) |
| `~/.config/dform/credentials/KIND/NAME` | the operator's credential `KIND:NAME` (under `$XDG_CONFIG_HOME`, or `DFORM_CREDENTIALS`); `age/NAME` an age identity that opens a master sealed to its recipient |
| `~/.cache/dform/` | git mirrors, compiled wasm providers, the language server's read-only files (under `$XDG_CACHE_HOME`) |
