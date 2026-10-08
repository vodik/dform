# The OVHcloud provider

`dform-provider-ovh` manages the Public Cloud of one OVH project
(instances, SSH keys, block volumes, private networks and their subnets,
users with S3 credentials, S3 containers) and the records of DNS zones OVH
hosts, over the OVH API (`/cloud/project/{serviceName}/...`,
`/domain/zone/{zone}/record`). It is a native provider: dform starts it
and speaks the plugin protocol to it.

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
use ovh { endpoint = "ovh-ca", project = "vodik" }
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

Each type is an object the API gives an identity. A relationship whose
state the API keeps on another object is an attribute of that object, not
a type of its own: a volume's attachment is the volume's `instance`, an
instance's private networks its `networks`, a user's S3 credential its
`s3_access_key` and `s3_secret_key`.

| type                        | the API's object                                        | key                     |
|-----------------------------|---------------------------------------------------------|-------------------------|
| `ovh.instance`              | `/cloud/project/{p}/instance/{id}`                      | name, region            |
| `ovh.ssh_key`               | `/cloud/project/{p}/sshkey/{id}`                        | name                    |
| `ovh.volume`                | `/cloud/project/{p}/volume/{id}`                        | name, region            |
| `ovh.network`               | `/cloud/project/{p}/network/private/{id}`               | name                    |
| `ovh.subnet`                | `/cloud/project/{p}/network/private/{id}/subnet/{id}`   | network, region, range  |
| `ovh.cloud_project_user`    | `/cloud/project/{p}/user/{id}`                          | description             |
| `ovh.storage_container`     | `/cloud/project/{p}/region/{r}/storage/{name}`          | region, name            |
| `ovh.domain_record`         | `/domain/zone/{zone}/record/{id}`                       | zone, subdomain, type, target |

`ovh.instance`

| attribute    | type              |                                     |
|--------------|-------------------|-------------------------------------|
| `name`       | string, required  | unique in the project; changes in place |
| `region`     | string, required  | `ca-east-tor`, `BHS5`, ...; replaces |
| `flavor`     | string, required  | the flavor's name, `b2-7`; replaces  |
| `image`      | string, required  | the image's name in the region, `Ubuntu 24.04`; replaces |
| `ssh_key`    | ref(ovh.ssh_key)  | `ssh_key = admin`; replaces          |
| `user_data`  | string, sensitive | cloud-init; replaces                 |
| `networks`   | set(ref(ovh.network)) | `networks = [lab]`; replaces    |
| `id`         | computed          |                                     |
| `public_ip`  | ip, computed      | once the instance has one           |
| `private_ip` | ip, computed, nullable | its first private address      |
| `private_ips`| map(ip), computed | its address on each private network, by the network's id |
| `status`     | string, computed  | `ACTIVE` once it runs               |

A replacement deletes the old instance first (its name is its key). Plan
checks the flavor and the image against what the region offers, naming
what it does offer. An instance on private networks is made with an
interface on the public network and one on each of them, in its region
(each needs a subnet there); a network not in its region is refused
naming the regions it is in. `server.private_ips[lab.id]` is its address
on `lab`.

The API never answers an instance's user data: the schema marks it
`write_only` (R-106), so dform keeps the digest of what it applied (never
the text) in state beside the instance, and Plan compares the program's
user data with it: a different one replaces the instance, on any machine
that reads the state. With nothing kept (an instance made elsewhere) the
user data is taken as unchanged.

`ovh.ssh_key`: `name` and `public_key`, both required, a change to either
replaces it; `id` computed. Its name is its key.

`ovh.volume`

| attribute     | type                 |                                      |
|---------------|----------------------|--------------------------------------|
| `name`        | string, required     | unique in its region; changes in place |
| `region`      | string, required     | replaces                             |
| `size`        | bytes, required      | whole GiB (`50Gi`); grows in place, a smaller one replaces |
| `type`        | enum                 | `classic` (when not set), `high-speed`, `high-speed-gen2`, each with `-luks`; replaces |
| `description` | string               | changes in place                     |
| `image`       | string, write-only   | the region's image by name, for a bootable volume; replaces |
| `snapshot`    | string, write-only   | a snapshot's id to make it from; replaces |
| `instance`    | ref(ovh.instance)    | `instance = server`: attached; changes in place |
| `id`, `status`| computed             | `available`, `in-use`                |

The attachment is the volume's: set, the volume is attached once it is
`available`; cleared, detached; changed, detached and attached to the new
one; the reference orders the volume after the instance, and a volume
attached when it is deleted is detached first. One instance at a time
(multi-attach volumes are not served). The API never answers the image or
the snapshot, so dform keeps their digests, as an instance's user data.

`ovh.network`: a private network, a VLAN of the vRack the project is on.
`name` (in place), `vlan_id` (0 when not set; replaces), `regions` (every
region of the project when not set; one added is added in place, one left
replaces it); computed `id` (`pn-..._42`), `status` and `regions_status`.
The vRack is the account's, attached to the project in the OVH control
panel, outside dform, as a DNS zone is: on a project without one, Plan
refuses the network naming what is missing:

```
plan ovh.network["lab"]: project 0123... is not on a vRack, and a private network is a VLAN of one: attach the account's vRack to the project in the OVH control panel (it is the account's, outside dform)
```

The public network is OVH's: a program never makes it, and an instance is
always on it.

`ovh.subnet`: `network = lab`, `region` (one of the network's), `range`
(inet, `10.0.0.0/24`; the API calls it `network`), `pool` (the addresses
instances are given, an `iprange`, both ends in it: the API's `start` and
`end`), `dhcp` (off when not set), `no_gateway` (a gateway at the range's
first address when not set); computed `gateway_ip`. Every change replaces
it (the API has none in place). Its remote id is `NETWORK/ID`.

```
resource ovh.subnet nodes {
  network = lab
  region = "BHS5"
  range = "10.42.0.0/24"
  dhcp = true
}
```

A subnet without a pool is given its range's hosts, as the OVH console
fills them in: from the first after the gateway (`.2`; `.1` with
`no_gateway = true`) to the last before broadcast (`.254`). Plan says
which (`pool = "10.42.0.2-10.42.0.254"` among the create's lines), and
Read answers the pool the API has as a computed value, so a program that
leaves it out plans clean. A pool the program writes is
`pool = "10.42.0.10-10.42.0.200"`; one outside the range's hosts, or
holding the gateway, is refused at plan. `start` and `end` are not
attributes: Plan refuses them naming `pool`. Both ends of an `iprange`
are in it: when ranges become `range(T)` (R-180) the pool is the
inclusive `"10.42.0.2..=10.42.0.254"`.

`ovh.cloud_project_user`: an OpenStack user of the project. `description`
(its key: the API makes up its username; replaces) and `roles` (a set of
the API's role names, `objectstore_operator`, `compute_operator`, ...;
changes in place); computed `username`, `status` (`creating`, then `ok`),
`s3_access_key` and `s3_secret_key`. Each user is given an S3 credential
when it is made. The secret is sensitive and held by the provider: the
API keeps it, Read and Apply answer only its label, and a provider
configured with it (`secret_key = backup.s3_secret_key` in its `use`
block, or another stack's output of it) gets the bytes by the protocol's
Reveal, which reads them from the API; dform never has them. The API
makes the user's OpenStack password and answers it once, when the user is
made: the provider drops it, and it is not an attribute.

`ovh.storage_container`: an S3 container of a region
(`/region/{r}/storage`; the older Swift containers of `/storage` are not
served). `region` and `name` (its key; both replace), `versioning` (on is
`enabled`, off again `suspended`; in place), `owner = backup` (the user
that owns it, the project's first S3 user when not set; replaces);
computed `virtual_host`. The API refuses to delete a container that has
objects.

`ovh.domain_record`: `zone`, `subdomain` (none for the apex), `type` (`A`,
`AAAA`, `CNAME`, `TXT`, `SRV`, `MX`), `target`, `ttl` (0, the zone's
default, when not set). Only `ttl` changes in place. The zone is refreshed
after every change. Its remote id, and its `id`, is `ZONE/ID`. A zone the
account does not host (OVH answers 404 for `/domain/zone/{zone}`) is
refused naming it, and where DNS says it is delegated, asked of the
machine's resolver (the first `nameserver` of /etc/resolv.conf;
`DFORM_OVH_RESOLVER=IP:PORT` names another):

```
apply ovh.domain_record["k8s"]: zone vodik.xyz is not hosted on this OVH account (its nameservers are ns1.digitalocean.com, ns2.digitalocean.com, ns3.digitalocean.com)
```

## Data sources

Tables the provider's schema declares (`extern_decl`, R-106), which a
program reads like any relation with no `extern` line:

```text
ovh.region(+project, -name, -status)
ovh.flavor(+region, -name, -vcpus: int, -ram: bytes, -disk: bytes)
ovh.image(+region, -name, -id, -distribution)

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
of its key (the table above): one this provider process made for the
same idempotency key is the answer, brought to the document (a user's S3
credential made, a volume attached); another is refused, to be adopted
(`adopt(r, ID)`) or renamed. `provider.created` answers by the same key,
so a Create dform gave up waiting for is adopted, not made twice. A 429 or
5xx answer is sent again with backoff (`[providers.ovh] retries`,
`backoff`).

## Not done

Managed Kubernetes, databases, load balancers, floating IPs, volume
snapshots and backups, fixed private addresses for an instance, and
multi-attach volumes are not in this provider yet.

## Testing

`cargo test --test provider_ovh` runs it against a fake OVH API
(`dform_provider_ovh::fake`, with a DNS resolver beside it). `OVH_INTEGRATION=1
OVH_CLOUD_PROJECT_SERVICE=<project> cargo test --test provider_ovh
the_real_account` lists the regions of the real account the machine's
configuration names, and changes nothing.
