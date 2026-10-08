# The Vault provider

`dform-provider-vault` reads secrets from HashiCorp Vault's KV version 2
secrets engine (and OpenBao's, which speaks the same API). It manages
nothing: it declares the location scheme `vault`, so a program reads a
secret it does not own the way it reads any document, into a secret
cell:

```dform
let signing: secret(string) = io.read("vault://kv/synapse/signing#key")
```

It is a native provider written with the SDK (`crates/dform-sdk`): every
request it makes goes through dform's HTTP client, so TLS, the proxy and
the token are the host's ("Writing a provider", docs/providers.md).

## Configure

Build it with the workspace (`cargo build --workspace` puts it at
`target/debug/dform-provider-vault`), name it in `dform.toml` and grant
it the credential its token is:

```toml
[providers.vault]
path = "~/src/dform/target/debug/dform-provider-vault"
credentials = ["bearer:vault"]
```

The token is the operator's, by name, never in the program: the file
`$XDG_CONFIG_HOME/dform/credentials/bearer/vault` holds it
(`DFORM_CREDENTIALS` names another directory), and the host sets it as
`Authorization: Bearer ..`, which Vault takes as `X-Vault-Token` (a
`header:NAME` credential holding `X-Vault-Token: hvs...` works too). The
provider names the credential and never sees its value.

The program says where Vault is:

```dform
use vault {
  address = "https://vault.example:8200"   # else VAULT_ADDR
  namespace = "ops"                        # Vault Enterprise, when used
  token = "bearer:vault"                   # the credential's name; this one unless written
}
```

AppRole logs in with a role id and a secret id instead of a token; the
secret id is a secret the program gives, revealed into the provider's
Configure and kept in its memory, as is the token the login answers:

```dform
input vault_secret_id: secret(string)
use vault {
  address = "https://vault.example:8200"
  approle = { role_id: "4b2e..", secret_id: vault_secret_id }   # mount: "approle" unless written
}
```

TLS is the host's: the machine's CA store and proxy (`HTTPS_PROXY`,
`NO_PROXY`). A Vault behind a private CA needs that CA in the machine's
store; a client certificate is a `tls:NAME` credential the host presents.

## Locations

| location | reads |
|---|---|
| `vault://MOUNT/PATH#KEY` | the key `KEY` of the secret at `PATH` in the KV v2 mount `MOUNT`, its current version: a string as it is, any other value as JSON |
| `vault://MOUNT/PATH` | the whole secret's data, as JSON (`json.decode(io.read(..))` reads its keys) |
| `vault://MOUNT/PATH?version=N#KEY` | version `N` (`#KEY?version=N`, as Vault's CLI writes it, too) |

Each read answers the version Vault names it by. The plan file records
it beside the secret's keyed digest (never the secret), and the read's
rows name it pinned, `vault://kv/synapse/signing?version=3#key`. `apply
PLAN` reads the secret again and refuses one that moved:

```
plan file plan.json is stale: re-evaluation after refresh does not reproduce its delta:
- io.read("vault://kv/synapse/signing#key"): version 3 in the plan, 4 now: it moved in its secret manager since the plan
Error: stale plan: run plan again
```

A new plan reads version 4 and shows what it changes. `dform secrets
list` lists the secret as `managed`, its generation the version read,
and `dform secrets rotate` says to rotate it in Vault.

A secret not written yet is "not yet": the apply waits on it (`waits on
vault://kv/app/later#key`), within `[io] wait`. These are errors, said at
the read:

- a key the secret does not have names the keys it has;
- a version deleted (`vault kv undelete` restores it) or destroyed;
- a token Vault refuses (403) names the policy capability to grant:
  `read` on `MOUNT/data/PATH`;
- a 429 or a 5xx is sent again under the provider's retry policy.

The token's policy needs `read` on `MOUNT/data/PATH` and nothing else:

```hcl
path "kv/data/synapse/*" { capabilities = ["read"] }
```

## Not yet

- KV version 1 mounts, dynamic secrets (database, PKI) and leases: a
  dynamic secret is issued per read and has no version to pin, so it
  belongs in a resource, not a location.
- A `vault.kv_secret` resource, for a team that keeps what dform derives
  in Vault too.
- A private CA for Vault alone (a CA-only credential): today the
  machine's store.
- A component build: native only.
