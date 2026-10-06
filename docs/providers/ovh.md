# The OVHcloud provider

`dform-provider-ovh` manages Public Cloud instances and SSH keys of one
OVH project, and the records of DNS zones OVH hosts, over the OVH API
(`/cloud/project/{serviceName}/...`, `/domain/zone/{zone}/record`). It is
a native provider: dform starts it and speaks the plugin protocol to it.

## Configure

Build it with the workspace (`cargo build --workspace` puts it at
`target/debug/dform-provider-ovh`) and name it in the project's
`dform.toml` until a registry serves it:

```toml
[providers]
ovh = { path = "~/src/dform/target/debug/dform-provider-ovh", timeout = "10m" }
```

`timeout` is how long dform waits for one call (60s by default). An
instance's Create waits for the instance to be ACTIVE, which takes one to
three minutes; with the default, the Create times out, dform finds the
instance by its name and adopts it, and a tick that reads its address
waits for it. A longer timeout makes the apply print less.

The program names the account, never a credential:

```text
provider ovh { endpoint = "ovh-ca", project = "vodik" }
```

- `endpoint`: `ovh-eu`, `ovh-ca`, `ovh-us` (or `kimsufi-*`,
  `soyoustart-*`, or a URL). Without it, `OVH_ENDPOINT`, else the
  `[default] endpoint` of `ovh.conf`.
- `project`: the Public Cloud project's id (its `serviceName`), or the
  description it has in the control panel. Without it,
  `OVH_CLOUD_PROJECT_SERVICE`. The provider reports the project's id as its
  account, so `expect_account = "0123..."` holds a deployment to it.

The credentials are the provider's own, read as OVH's SDKs read them:
`OVH_APPLICATION_KEY`, `OVH_APPLICATION_SECRET` and `OVH_CONSUMER_KEY` in
the environment, else the endpoint's section of `ovh.conf`:

```ini
[default]
endpoint=ovh-ca

[ovh-ca]
application_key=...
application_secret=...
consumer_key=...
```

read from `/etc/ovh.conf`, `~/.ovh.conf` and `~/.config/ovh/ovh.conf`
(`$XDG_CONFIG_HOME/ovh/ovh.conf`), a later file overriding an earlier one.
Make the keys at https://ca.api.ovh.com/createToken/ (or the `eu`/`us`
host), with GET, POST, PUT and DELETE on `/cloud/project/*` and, for DNS
records, `/domain/zone/*`.

## Resources

`ovh.instance`

| attribute    | type              |                                     |
|--------------|-------------------|-------------------------------------|
| `name`       | string, required  | unique in the project; changes in place |
| `region`     | string, required  | `ca-east-tor`, `BHS5`, ...; replaces |
| `flavor`     | string, required  | the flavor's name, `b2-7`; replaces  |
| `image`      | string, required  | the image's name in the region, `Ubuntu 24.04`; replaces |
| `ssh_key`    | ref(ovh.ssh_key)  | `ssh_key = admin`; replaces          |
| `user_data`  | string, sensitive | cloud-init; replaces                 |
| `id`         | computed          |                                     |
| `public_ip`  | ip, computed      | once the instance has one           |
| `private_ip` | ip, computed, nullable |                                |
| `status`     | string, computed  | `ACTIVE` once it runs               |

A replacement deletes the old instance first (its name is its key). Plan
checks the flavor and the image against what the region offers, naming
what it does offer.

The API never answers an instance's user data, so the provider keeps the
SHA-256 of what it sent (never the text) in `dform.state/cache/
ovh-user-data.json`, and Plan compares the program's user data with it: a
different one replaces the instance. With nothing kept (an instance made
elsewhere, a cache cleared, another machine) the user data is taken as
unchanged.

`ovh.ssh_key`: `name` and `public_key`, both required, a change to either
replaces it; `id` computed. Its name is its key.

`ovh.domain_record`: `zone`, `subdomain` (none for the apex), `type` (`A`,
`AAAA`, `CNAME`, `TXT`, `SRV`, `MX`), `target`, `ttl` (0, the zone's
default, when not set). Only `ttl` changes in place. The zone is refreshed
after every change. Its remote id, and its `id`, is `ZONE/ID`.

## Data sources

Tables a program declares with `extern` and reads like any relation:

```text
extern ovh.region(+project, -name, -status)
extern ovh.flavor(+region, -name, -vcpus: int, -ram: bytes, -disk: bytes)
extern ovh.image(+region, -name, -id, -distribution)

resource ovh.instance db {
  name = "db"
  region = "BHS5"
  flavor = "d2-2"
  image
} where ovh.image("BHS5", image, _, "Debian")
```

`ovh.flavor` lists the region's available flavors (RAM and disk as the
API counts them, MiB and GiB); `ovh.image` its active images, the
distribution being the first word of the image's name.

## What a Create that does not answer does

OVH's create calls are not idempotent. A Create first looks for an object
of its key (an instance's name in its region, a key's name, a record's
zone, subdomain, type and target): one this provider process made for the
same idempotency key is the answer; another is refused, to be adopted
(`adopt(r, ID)`) or renamed. `provider.created` answers by the same key,
so a Create dform gave up waiting for is adopted, not made twice. A 429 or
5xx answer is sent again with backoff (`[providers.ovh] retries`,
`backoff`).

## Not done

Block volumes and their attachments (`ovh.volume`,
`ovh.volume_attachment`), private networks, Managed Kubernetes and
databases are not in this provider yet.

## Testing

`cargo test --test provider_ovh` runs it against a fake OVH API
(`dform_provider_ovh::fake`). `OVH_INTEGRATION=1
OVH_CLOUD_PROJECT_SERVICE=<project> cargo test --test provider_ovh
the_real_account` lists the regions of the real account the machine's
configuration names, and changes nothing.
