# The Tailscale provider

`dform-provider-tailscale` manages one tailnet over the Tailscale API v2:
its policy file (`tailscale.acl`), its auth keys (`tailscale.auth_key`),
its DNS settings (`tailscale.dns`) and the devices on it
(`tailscale.device`); its users are a data source (`tailscale.user`). It
is a native provider written with the SDK (`crates/dform-sdk`): every
request goes through dform's HTTP client ("Writing a provider",
docs/providers.md).

## Configure

Build it with the workspace (`cargo build --workspace` puts it at
`target/debug/dform-provider-tailscale`) and name it in `dform.toml`:

```toml
[providers]
tailscale = { path = "~/src/dform/target/debug/dform-provider-tailscale" }
```

The program names the tailnet, never a credential:

```dform
use tailscale { tailnet = "vodik.github" }
```

- `tailnet`: the tailnet's name, as the admin console's settings give it
  (`-` is the credential's own). Without it, `TAILSCALE_TAILNET`. The
  provider reports it as its account, so `expect_account =
  "vodik.github"` holds a deployment to it.
- `base_url`: the API, `https://api.tailscale.com` unless
  `TAILSCALE_BASE_URL` or this says otherwise.
- `credential`: a credential by name, below.

### Credentials

One of three, by Tailscale's own names (what its Terraform provider
reads); two given is an error naming each:

- An OAuth client, `TAILSCALE_OAUTH_CLIENT_ID` and
  `TAILSCALE_OAUTH_CLIENT_SECRET` in the environment. The provider mints
  a token from it (`/api/v2/oauth/token`, the client credentials grant)
  and keeps it for the run. The client's scopes are what the provider
  may do: `policy_file` for `tailscale.acl`, `auth_keys` for
  `tailscale.auth_key`, `dns` for `tailscale.dns`, `devices:core` (and
  `devices:routes` for `routes`) for `tailscale.device`, `users:read`
  for `tailscale.user`; a refusal names the scope the call needs. A key
  an OAuth client makes must carry a tag the client may apply: the plan
  refuses an untagged one.
- An API access token, `TAILSCALE_API_KEY`.
- A credential by name the host applies, `credential =
  "bearer:tailscale"`, granted in `dform.toml` (`[providers.tailscale]
  credentials = ["bearer:tailscale"]`) and kept in the operator's file
  (`$XDG_CONFIG_HOME/dform/credentials/bearer/tailscale`): its value
  never reaches the provider.

## Types

| type | identity | a change | destroy |
|---|---|---|---|
| `tailscale.acl` | the tailnet | in place | writes the default policy, and says so |
| `tailscale.auth_key` | the API's id; found by its `description` | any change makes a new key, then revokes the old | revokes it |
| `tailscale.dns` | the tailnet | in place, each part that differs | writes the defaults, and says so |
| `tailscale.device` | the device's id; adopted by its `hostname` | in place | removes it from the tailnet |

### `tailscale.acl`

`policy` is the policy file as the program renders it:

```dform
resource tailscale.acl policy {
  policy = json.encode({
    tagOwners: { "tag:k8s": ["autogroup:admin"], "tag:admin": ["autogroup:admin"] },
    acls: [{ action: "accept", src: ["tag:admin"], dst: ["tag:k8s:22,6443"] }],
  })
}
```

Read answers the file the tailnet has (HuJSON on the wire) in the form
`json.encode` writes, JSON with each object's keys sorted and no space,
and the provider compares in that form: a comment, a trailing comma or
another order written in the admin console is no change, an edited rule
is drift, and the next apply writes the program's policy back. A policy
written otherwise is refused at the plan, naming `json.encode`. A write
carries `If-Match` with the ETag of the file it read, so an edit made in
between is not overwritten: the write is refused and sent again over
what the file is then.

A tailnet always has a policy file. A create writes the program's over
the tailnet's default, and refuses one somebody wrote: adopt it
(`adopt(policy, "vodik.github")`), and the plan shows what the program
changes in it. A destroy writes the default back (every device reaches
every other; SSH to one's own devices with a check) and says so under
the change.

### `tailscale.auth_key`

```dform
resource tailscale.auth_key nodes {
  description = "k3s nodes"
  reusable = true
  ephemeral = true
  preauthorized = true
  tags = ["tag:k8s"]
  expiry = 7d
}
```

`reusable`, `ephemeral`, `preauthorized`, `tags` (a set) and `expiry` (at
most 90d, the API's default) are given at creation: any change makes a
new key first and then revokes the old one. Revoking a key leaves the
devices that joined with it on the tailnet. A key that expired or was
revoked in the console is gone, and the next plan makes another. `id` is
the API's.

`key` is sensitive and held by the provider. The API answers it once, to
the create; the provider keeps it in memory for the run and gives it to
dform's engine alone, which reveals it into the call that takes it (R-45).
State, the plan file, the audit log and every message have its label,
`tailscale.auth_key/nodes#key`, never the bytes. A run after the one that
made the key cannot reveal it: the API keeps no copy, and the reveal is
refused saying so. A node that needs a key later needs a new key (change
its description, or taint it).

The key is for a node's first boot, in its cloud-init: say so, so a new
key (a rotation, an expiry) does not replace the servers it is in:

```dform
resource ovh.instance server {
  user_data = "#cloud-config\nruncmd:\n  - tailscale up --authkey=${nodes.key} --advertise-tags=tag:k8s\n"
  ..
}
lifecycle(server, "bootstrap", "user_data")
```

`bootstrap` sends the user data when the instance is made and never
compares it after (docs/reference.md, "Lifecycle"). Not yet: the engine
reveals a held secret into a provider's Configure only, so a key in
another provider's attribute is not revealed there yet ("Not yet").

### `tailscale.dns`

```dform
resource tailscale.dns dns {
  nameservers = ["1.1.1.1"]
  search_paths = ["home.vodik.xyz"]
  split = { "home.vodik.xyz": ["192.168.1.1"] }
}
```

`nameservers` (the ones every device asks), `magic_dns`, `search_paths`
and `split` (a domain to the nameservers that answer for it) are four
calls of the API and one object here: an update writes the parts that
differ. `magic_dns` is compared only where the program writes it. Like
the policy file the settings always exist: a create refuses settings
somebody wrote (adopt them by the tailnet's name), and a destroy writes
the defaults (no nameservers, no search paths, no split domain, MagicDNS
on) and says so.

### `tailscale.device`

The tailnet makes a device when a node joins it with an auth key, so a
program does not create one: the plan refuses it, `a device joins the
tailnet with an auth key; adopt it`. A program adopts it by its hostname
and manages it by its id from then on:

```dform
resource tailscale.device server {
  hostname = "k3s-1"
  tags = ["tag:k8s"]
  routes = ["10.0.0.0/24"]
}
adopt(server, "k3s-1")
```

A hostname is not unique, the id is: while a replaced node's old device
is still listed, two devices have its hostname, and the adopt is refused
naming both ids (`2 devices have the hostname "k3s-1": n1.. (last seen
..), n2..`), never one picked. Make the nodes' key `ephemeral`: the old
device leaves the tailnet once it is offline, and the adopt holds again.

A program writes `tags`, `routes` (the subnet routes approved, of those
the device advertises), `authorized` and `name` (its machine name, the
first label of its MagicDNS name), each compared only where it writes
it. `hostname` is the node's own: a change is refused. `id`,
`addresses` (IPv4 then IPv6), `os` and `last_seen` are the device's.
Removing the resource from the program removes the device from the
tailnet; `lifecycle(tailscale.device["server"], "retain")`, by its
address, lets it go instead (docs/reference.md, "Lifecycle").

`dform status` asks each device's health: `healthy` connected (`connected
at 100.64.0.1`), `degraded` not authorized, not connected (`not
connected, last seen ..`) or no longer on the tailnet.

## Data sources

`tailscale.user(+tailnet, -login, -role)`: the tailnet's users, by login
name and role (`owner`, `admin`, `member`, ..):

```dform
deny "${l} is an admin" where tailscale.user("vodik.github", l, "admin")
```

## Not yet

- A key in another provider's attribute (an instance's `user_data`): the
  engine reveals a secret a provider holds into a Configure only, and a
  key interpolated into a string waits on a value dform never has. It
  needs the engine to reveal the key into the call that writes the
  attribute, and nothing at a later run that keeps it (`bootstrap`).
- The devices the tailnet lists, adopted or not, as facts a policy reads
  (`d in tailscale.device`): R-196.
- Removal of a device from the program letting it go by default (a
  type's default `retain`, `lifecycle(d, "destroy")` to remove it): a
  removal removes it today.
- Posture attributes, device key expiry, invites, webhooks, the tailnet's
  settings; a component build (native only).
