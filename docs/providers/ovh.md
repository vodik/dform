# The OVHcloud provider

`dform-provider-ovh` manages the Public Cloud of one OVH project
(instances, SSH keys, block volumes, private networks and their subnets,
users and their S3 credentials, S3 containers) and the records of DNS zones OVH
hosts, over the OVH API (`/cloud/project/{serviceName}/...`,
`/domain/zone/{zone}/record`). It is a native provider: dform starts it
and speaks the plugin protocol to it. It sends each request through the
host's HTTP, which signs it with the account's keys the provider never
holds, so dform holds its connection to the API, paces its requests, each class of
endpoint in a window of its own (`instance`, `network`, `volume`,
`domain`, ..: a project's kind of object, else the API's first path
segment), and traces each under the call it is made for.

## Configure

Build it with the workspace (`cargo build --workspace` puts it at
`target/debug/dform-provider-ovh`) and name it in the project's
`dform.toml` until a registry serves it:

```toml
[providers.ovh]
path = "~/src/dform/target/debug/dform-provider-ovh"
timeout = "10m"
```

`timeout` is how long dform waits for one call (60s by default). An
instance's Create waits for the instance to be ACTIVE, which takes one to
three minutes; with the default, the Create times out, dform finds the
instance by its name and adopts it, and a tick that reads its address
waits for it. A longer timeout makes the apply print less.

Built as a component (`cargo xtask components` puts it at
`target/wasm32-wasip3/release/dform_provider_ovh.wasm`, dform built with
`--features wasm`), it reads no environment and no file, so its
credentials are the keys the host signs with or the program's `token`
(below), and it is granted `wasi:filesystem` for now: dform's language
front-end, which parses its schema, links it.

```toml
[providers.ovh]
path = "target/wasm32-wasip3/release/dform_provider_ovh.wasm"
allow = ["wasi:filesystem"]
credentials = ["signed:environment"]
```

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

### Credentials

An application key and secret and a consumer key sign each call, and
dform holds them; the provider's signer (`crates/dform-provider-ovh/
signer`, natively served beside the provider, as a component
`dform_provider_ovh_signer.wasm` beside its own) signs: the provider
never has them. dform.toml grants them by name, a `signed` credential
(docs/providers.md, "Grants and credentials"):

```toml
[providers.ovh]
credentials = ["signed:environment"]   # OVH_APPLICATION_KEY, .._SECRET, OVH_CONSUMER_KEY, OVH_ENDPOINT, else an ovh.conf
# or ["signed:lab"]: the file ~/.config/dform/credentials/signed/lab
```

`signed:environment` is the environment's `OVH_APPLICATION_KEY`,
`OVH_APPLICATION_SECRET` and `OVH_CONSUMER_KEY` (and `OVH_ENDPOINT`),
and, with none of them set, the first `ovh.conf` there is where
python-ovh looks (`./ovh.conf`, `$XDG_CONFIG_HOME/ovh/ovh.conf`,
`~/.config/ovh/ovh.conf` when `XDG_CONFIG_HOME` is not set,
`~/.ovh.conf`, `/etc/ovh.conf`), read as OVH's own clients read it
(python-ovh, the ovhcloud CLI, the Terraform provider): the section its
`[default] endpoint` names. The operator's file is an `ovh.conf`, or a
section's lines alone:

```ini
endpoint=ovh-ca
application_key=...
application_secret=...
consumer_key=...
```

Make them at https://ca.api.ovh.com/createToken/ (or the `eu`/`us`
host), with unlimited validity and the rights below. Each call is
stamped with the API's time: its endpoint named, the signer asks dform
to fetch its clock once (`GET ENDPOINT/auth/time`) and signs at dform's
time and the difference; no endpoint named, at dform's time. A consumer key the API no longer takes is said as "the
consumer key for ovh-ca expired or was revoked", with where to make one.
Configure reads the key's validity (`GET /auth/currentCredential`, kept
in dform's cache for that credential's name) and warns when it expires
within a week. Outside a program (`dform provider check`) it also says
which form is in use, when the key expires, and each right below the key
lacks, each a line of the check's: `note  Configure: provider ovh: the
consumer key for ovh-ca lacks POST /domain/zone/*`. Without the grant,
the three keys in `ovh.conf` or the environment are not read: the error
names them and the grant.

The other forms are the provider's own, read as OVH's SDKs (python-ovh,
go-ovh) read them: each key from the environment (`OVH_` and the key's
name in capitals), else from the section of `ovh.conf` named by the
endpoint, read from these files, a later one overriding an earlier one
key by key:

1. `/etc/ovh.conf`
2. `~/.ovh.conf`
3. `./ovh.conf`, in the working directory: the directory dform runs in,
   or its `-C` directory.

A working directory's `ovh.conf` holds secrets: never commit it (name it
in the repository's `.gitignore`). One form is given: two, the
environment's keys counted with the file's and the grant with them, is
an error naming each.

A service account's client id and secret (`OVH_CLIENT_ID`,
`OVH_CLIENT_SECRET`). The provider mints a bearer token from them with
OAuth2's client-credentials grant (scope `all`) at the endpoint's token
URL (`https://ca.ovh.com/auth/oauth2/token` for `ovh-ca`, `www.ovh.com`
for `ovh-eu`, `us.ovhcloud.com` for `ovh-us`, `/auth/oauth2/token` at a
URL endpoint's host), keeps it until a minute
before it expires, and mints another when a call is refused with a 401.
The token does not outlive the run. Make the account in the OVH manager
under API > Service accounts; its rights are the IAM policies that name
it, and the kimsufi and soyoustart endpoints do not take one:

```ini
[default]
endpoint=ovh-ca

[ovh-ca]
client_id=...
client_secret=...
```

A bearer token minted elsewhere (`OVH_ACCESS_TOKEN`), as go-ovh takes
it: each call carries it as it is, and nothing mints another, so when it
expires the call is refused saying so. For a run, not for a machine's
configuration:

```ini
[ovh-ca]
access_token=...
```

Or the program gives that token, `use ovh { endpoint = "ovh-ca", project
= "vodik", token = env.var("OVH_ACCESS_TOKEN") }`: dform holds it in the
host, which applies it to each call, and the provider has only its name
(docs/providers.md, "Grants and credentials"). It is used in place of any
form the environment or `ovh.conf` gives.

The rights the provider needs, and no others; a type that calls another
path adds its line:

| method                  | path                |
|-------------------------|---------------------|
| GET                     | `/cloud/project`    |
| GET, POST, PUT, DELETE  | `/cloud/project/*`  |
| GET, POST, PUT, DELETE  | `/domain/zone/*`    |

## Resources

Each type is an object the API gives an identity, its `id`: state holds
it, and an update goes to it. An attribute replaces the object only
where the API cannot change it on its id. A relationship whose
state the API keeps on another object is an attribute of that object, not
a type of its own: a volume's attachment is the volume's `instance`, an
instance's private networks its `networks`. A user's S3 credentials are
not: each has a secret to reveal and a key to rotate on its own, so each
is an object, `ovh.s3_credential`. When a relationship may fold into an
attribute is docs/grammar.md's folding rule ("When a relationship
folds").

The last column is what a Create that timed out is found by (the
schema's `type_lookup`, see "What a Create that does not answer does").

| type                        | the API's object                                        | found by                |
|-----------------------------|---------------------------------------------------------|-------------------------|
| `ovh.instance`              | `/cloud/project/{p}/instance/{id}`                      | name, region            |
| `ovh.ssh_key`               | `/cloud/project/{p}/sshkey/{id}`                        | name                    |
| `ovh.volume`                | `/cloud/project/{p}/volume/{id}`                        | name, region            |
| `ovh.network`               | `/cloud/project/{p}/network/private/{id}`               | name                    |
| `ovh.subnet`                | `/cloud/project/{p}/network/private/{id}/subnet/{id}`   | network, region, range  |
| `ovh.cloud_project_user`    | `/cloud/project/{p}/user/{id}`                          | description             |
| `ovh.s3_credential`         | `/cloud/project/{p}/user/{id}/s3Credentials/{access}`   | (made under its key)    |
| `ovh.storage_container`     | `/cloud/project/{p}/region/{r}/storage/{name}`          | region, name            |
| `ovh.domain_record`         | `/domain/zone/{zone}/record/{id}`                       | zone, subdomain, type, target |

`ovh.instance`

| attribute    | type              |                                     |
|--------------|-------------------|-------------------------------------|
| `name`       | string, required  | unique in the project; changes in place |
| `region`     | string, required  | `ca-east-tor`, `BHS5`, ...; replaces |
| `flavor`     | string, required  | the flavor's name, `b2-7`; a larger one in place (a resize: the instance reboots), a smaller one replaces |
| `image`      | string, required  | the image's name in the region, `Ubuntu 24.04`; replaces |
| `ssh_key`    | ovh.ssh_key       | `ssh_key = admin`; replaces          |
| `user_data`  | string, sensitive | cloud-init; replaces                 |
| `networks`   | set({ network: ovh.network, ip?: ip }) | `networks = [lab]`, or `[{ network = lab, ip = .. }]` for a fixed address; changes in place, but a new address replaces |
| `id`         | computed          |                                     |
| `public_ip`  | ip, computed      | once the instance has one           |
| `private_ip` | ip, computed, nullable | its address on the first of its private networks, by id, as `private_ips` has it |
| `private_ips`| map(ip), computed | its address on each private network, by the network's id: its own port's, else the first port's |
| `ports`      | map, computed     | its own port on each private network, by the network's id: `id`, `ipv4`, and `fixed` where the program asked the address |
| `status`     | string, computed  | `ACTIVE` once it runs               |

The API changes an instance's name on its id (`PUT`), and its flavor
and networks by calls of their own on the id. A flavor no smaller than
the one it has (no fewer vCPUs, no less RAM or disk) is a resize
(`POST /resize`): the plan is a `~` naming the old flavor and the new,
and the Apply waits for the instance to be ACTIVE on it again, saying
`RESIZE`, and notes that it rebooted. The API resizes only up, so a
smaller flavor replaces the instance, and the plan says why under it
(`replaced: flavor d2-2 is smaller than b2-7; the API resizes an
instance only to a larger one`); planned
without the account (offline), a flavor change is a `~`, and an Apply
of a smaller one is refused naming both. A network added is an
interface attached, one left an interface detached (`POST`/`DELETE
/interface`), each a line of the plan (`+ networks[lab]`) and a note of
the Apply. Its image changes only by a reinstall, which erases its
disk, so a new one replaces it. A replacement deletes the old instance
first (it is found by its name). Plan checks the flavor and the image
against what the region offers, naming what it does offer. An instance
on private networks is made with an interface on the public network and
one on each of them, in its region (each needs a subnet there); a
network not in its region is refused naming the regions it is in.
`server.private_ips[lab.id]` is its address on `lab`.

The API marks no port as the instance's, and an attach repeated makes
another port on the same network (below). The provider keeps the port
it made under `ports`: the one on each network an instance is made
with, and the one an Apply attached, by id (`server.ports[lab.id].id`).
dform keeps the computed values in state and tells the provider's Read
and Apply what it kept (docs/reference.md, "Providers are processes"),
so a Read finds that port by its id among the others on the network,
and its address is the instance's there. A port it kept that is gone
(detached outside dform) is absent from `ports`, never carried over; a
network whose port it did not make (an instance adopted, or made
before ports were kept) has none in `ports`, and its address there is
the first port's.

An Apply that changes the instance brings its networks to one port
each, against its ports as the API lists them then, not as the plan
read them:

- a network it has no port on: one attached, and kept as its own;
- its own port there: kept, and any other port on the network detached,
  each a note naming its own;
- no port known as its own (no `ports` kept, or the one kept is gone)
  and one port there: that one taken as its own, said;
- no port known as its own and several there: the Apply is refused
  before anything changes, naming each port and its address, so a
  person chooses which to keep; it never keeps one at random;
- a network left: its own port detached, and a port that is not its
  own left as it is, said; with none known, its one port detached, and
  several refused as above.

```
apply ovh.instance["vm"]: network pn-1000123_42 has 3 ports and none is known as its own: if-a (10.0.0.21), if-b (10.0.0.22), if-c (10.0.0.23); it keeps one port on a network, and which one is not dform's to guess: detach the others (`DELETE /cloud/project/0123.../instance/6f8b.../interface/PORT`, or in the OVH control panel), keeping the one in use, and apply again: the one left is taken as its own
```

Ports beside its own are found only by an Apply of the instance: the
plan reads its networks as a set, so a second port on a network is no
change to plan.

An instance's address on a private network is DHCP's unless the program
asks for one: an entry of `networks` is a network or `{ network, ip }`,
and the port on that network is made with that address (the create's
`networks[].ip`, an attach's `ip`), DHCP left on for the rest. The
program knows the address before the instance exists, so its user data
can carry it:

```dform
let range = "10.42.0.0/24"

resource ovh.subnet nodes {
  network = lab
  region = "BHS5"
  range
  pool = "10.42.0.100..=10.42.0.200"
}

resource ovh.instance server {
  name = "server"
  region = "BHS5"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
  networks = [{ network = lab, ip = inet.host(range, 9) }]
  user_data = "#!/bin/sh\nexec k3s server --node-ip ${inet.host(range, 9)}\n"
}
```

`inet.host(range, 9)` is `10.42.0.10`, and `server.private_ips[lab.id]`
is that address, read from the port. The port is the instance's own from
its creation, `fixed` in `ports`, and while it has the address Read
answers the entry as the program writes it, so the plan after the apply
is clean.

- Plan checks that the address is a host of a subnet of the network in
  the instance's region (not the range's network address, broadcast or
  gateway), naming the network's subnets otherwise; a network the same
  plan makes, or one with no subnet in the region yet, is checked by the
  Apply's create instead, with the same words:

  ```
  refused  plan ovh.instance server: networks: 10.9.0.5 is no host of a subnet of network pn-1000123_42 in BHS5 (nor its gateway): its subnets are 10.42.0.0/24 in BHS5
  ```

- An address is one port's. Two instances asking one on one network are
  refused at plan, naming both; one an instance outside the run holds
  (another stack's, the console's) is the API's refusal at Apply:

  ```
  refused  plan ovh.instance agent: networks: 10.42.0.10 on network pn-1000123_42 is asked by both ovh.instance agent and ovh.instance server, and an address is one port's: give each instance its own
  ```

- No API call changes a port's address. A new one, or one asked on a
  network where DHCP gave the instance its address, replaces the
  instance, and the plan says why (`replaced: its address on network
  pn-1000123_42 changes from 10.42.0.10 to 10.42.0.11; no API call
  changes a port's address`); an address no longer asked changes
  nothing (the port keeps it); a network added with an address is an
  interface attached with it, in place. A replacement made first
  (`lifecycle(vm, "create_first")`) that asks the old instance's address
  deletes the old one first, and says so.
- The API takes an address inside the subnet's pool, where DHCP may have
  given it to another port: keep fixed addresses outside the pool, as
  `nodes` does above.

A project whose nodes find their address at boot moves to this once: a
node takes its index, `let address = inet.host(range, 10 + index)`, its
`networks` entry is `{ network = lab, ip = address }` and its user data
names the address as a literal (k3s's `--node-ip`), agents join the
server over the vRack (`K3S_URL=https://${server_ip}:6443`, the server's
address made the same way), and a deny holds every index's address
inside the range and outside the pool. Each node is replaced once: its
user data changes.

The API never answers an instance's user data: the schema marks it
`write_only`, so dform keeps the digest of what it applied (never
the text) in state beside the instance, and Plan compares the program's
user data with it: a different one replaces the instance, on any machine
that reads the state. With nothing kept (an instance made elsewhere) the
user data is taken as unchanged.

`ovh.ssh_key`: `name` and `public_key`, both required, a change to either
replaces it (the API has no update for a key); `id` computed.

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
| `instance`    | ovh.instance         | `instance = server`: attached; changes in place |
| `id`, `status`| computed             | `available`, `in-use`                |

The attachment is the volume's: set, the volume is attached once it is
`available`; cleared, detached; changed, detached and attached to the new
one; the reference orders the volume after the instance, and a volume
attached when it is deleted is detached first. Each is against the
volume as the API answers it then: one on its way (`attaching`,
`detaching`) is waited for first, an attachment already as asked sends
nothing, and only an instance it is attached to is detached from. One instance at a time
(multi-attach volumes are not served). The API never answers the image or
the snapshot, so dform keeps their digests, as an instance's user data.

`ovh.network`: a private network, a VLAN of the vRack the project is on.
`name` (in place), `vlan_id` (0 when not set; replaces), `regions` (every
region of the project when not set; one added is added in place, one left
replaces it, which the plan says); computed `id` (`pn-..._42`), `status` and `regions_status`.
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
instances are given, a `range(ip)`, both ends in it: the API's `start` and
`end`), `dhcp` (off when not set), `no_gateway` (a gateway at the range's
first address when not set); computed `gateway_ip`. Every change replaces
it (no update call for a subnet is known). Its remote id is `NETWORK/ID`.

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
which (`pool = "10.42.0.2..=10.42.0.254"` among the create's lines), and
Read answers the pool the API has as a computed value, so a program that
leaves it out plans clean. A pool the program writes is
`pool = "10.42.0.10..=10.42.0.200"` (or `"10.42.0.10"..="10.42.0.200"`);
one outside the range's hosts, or holding the gateway, is refused at
plan. `start` and `end` are not attributes: Plan refuses them naming
`pool`.

`ovh.cloud_project_user`: an OpenStack user of the project. `description`
(what it is found by: the API makes up its username; no call to change
it is known, so it replaces) and `roles` (a set of
the API's role names, `objectstore_operator`, `compute_operator`, ...;
changes in place); computed `username` and `status` (`creating`, then
`ok`). The API makes the user's OpenStack password and answers it once,
when the user is made: the provider drops it, and it is not an attribute.

`ovh.s3_credential`: an S3 credential of a user, `user = backup` (the
user; another one replaces it); computed `access`, the access key the
API makes, and `secret`. A user has as many as the program writes, each
revealed by its own name; its remote id is `USER/ACCESS`. The secret is
sensitive and held by the provider: the API answers it to the create and
to a call of its own (`POST …/s3Credentials/{access}/secret`), never to a
read (the schema's `type_created_once`, docs/reference.md), so Read and
Apply answer only its label, the same every time (no change, never
empty), and a provider configured with it (`secret_key =
backup_key.secret` in its `use` block, or another stack's output of it)
or `dform output TARGET NAME --reveal` of an output of it gets the bytes
by the protocol's Reveal, which asks the API; dform never keeps them.
Nothing of a credential changes in place. A replacement is made before
the old one goes (`type_replace` `create_first`), so what reads it moves
first; to rotate a key, write another credential, move what reads it,
then remove the first.

```dform
resource ovh.cloud_project_user backup {
  description = "backups"
  roles = ["objectstore_operator"]
}

resource ovh.s3_credential backup_key {
  user = backup
}

output backup_secret: secret(string) = backup_key.secret
```

Until 2026-10-15 a user was made with one S3 credential, its
`s3_access_key` and `s3_secret_key`. Those attributes are gone: a program
that read them writes the credential (above) and reads
`backup_key.access` and `backup_key.secret` instead. The credential the
user was made with is still there; to keep it rather than make another,
adopt it by its remote id, the user's id and the access key state has
for it (`adopt(backup_key, "482913/0123…")`); otherwise the apply makes a
new one, and the old one is removed in the OVH control panel once nothing
uses it.

`ovh.storage_container`: an S3 container of a region
(`/region/{r}/storage`; the older Swift containers of `/storage` are not
served). `region` and `name` (what it is found by; S3 never renames a
container, so both replace), `versioning` (on is
`enabled`, off again `suspended`; in place), `owner = backup` (the user
that owns it, the project's first S3 user when not set; replaces);
computed `virtual_host`. The API refuses to delete a container that has
objects.

`ovh.domain_record`: `zone`, `subdomain` (none for the apex), `type` (`A`,
`AAAA`, `CNAME`, `TXT`, `SRV`, `MX`), `target`, `ttl` (0, the zone's
default, when not set). `subdomain`, `target` and `ttl` change in place,
one `PUT` on the record's id, so a new address is a `~` and the name
keeps resolving; a ttl the program does not write is left as it is.
`zone` and `type` replace it. The zone is refreshed after every change. Its remote id, and its `id`, is `ZONE/ID`. A zone the
account does not host (OVH answers 404 for `/domain/zone/{zone}`) is
refused naming it, and where DNS says it is delegated, asked of the
machine's resolver (the first `nameserver` of /etc/resolv.conf;
`DFORM_OVH_RESOLVER=IP:PORT` names another):

```
apply ovh.domain_record["k8s"]: zone vodik.xyz is not hosted on this OVH account (its nameservers are ns1.digitalocean.com, ns2.digitalocean.com, ns3.digitalocean.com)
```

## Data sources

Tables the provider's schema declares (`extern_decl`), which a
program reads like any relation with no `extern` line:

```text
ovh.region(+project, -name, -status)
ovh.flavor(+region, -name, -vcpus: int, -ram: bytes, -disk: bytes)
ovh.image(+region, -name, -id, -distribution)
ovh.zone(+name, -id, -nameservers)

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

`ovh.zone` is a DNS zone the account hosts (`GET /domain/zone/{zone}`):
its id the name the API keys it by, what a record's `zone` names, and
its nameservers as the API lists them; a zone the account does not host
has no row. A record whose clause reads a missing row is simply not
wanted, which is correct and silent, so the deny is what makes the
absence loud; the refusal above stays as the backstop:

```dform
let zone = "vodik.xyz"
deny "the zone ${zone} is not hosted on this account" where not { ovh.zone(zone, _, _) }

resource ovh.domain_record k8s {
  zone = z
  subdomain = "k8s-lab"
  type = "A"
  target = "51.79.29.179"
} where ovh.zone(zone, z, _)
```

## What it asks the API

A Read asks its object by its id: an instance, an SSH key, a volume, a
private network, a user, a record one `GET`; an S3 container `GET
/region/{region}/storage/{name}`; an S3 credential its user's
credentials; a subnet its network's subnets, as the API has no `GET` of
one. An instance's Read asks the instance, its ports
(`…/instance/{id}/interface`) and the project's private networks, which
name the ports' networks, at once.

A list is asked once per session and run, shared by every Read, Plan
and Apply of the session, and the callers asking it at the same time
wait on the one request: a region's flavors and images, the regions,
the private networks, each network's subnets, the public network, a
region's instances and volumes, the SSH keys and the users (what a
Create is found by), a user's S3 credentials, and whether the project
is on a vRack. A write (`POST`, `PUT`, `DELETE`) forgets the
lists of its kind of object, whatever its answer, so the next one asked
is as the API is after it. A failure is not kept. The OVH API answers a
list in seconds; a run pays it once.

Plan checks an instance's flavor and image, and a bootable volume's
image, against the region's lists only where the program asks one the
object does not have there: the flavor and image Read answered are the
API's, and a Plan of an instance as it is asks nothing.

## What a Create that does not answer does

OVH's create calls are not idempotent. A Create first looks for an object
of its content, what the table above says it is found by (the schema's
`type_lookup`; not its identity, and not what replaces it): one
this provider process made for the
same idempotency key is the answer, brought to the document (a user's
roles set, a volume attached; an instance owns the port on each network
it was made with, as its Create would have answered); another is
refused, to be adopted
(`adopt(r, ID)`) or renamed. `provider.created` answers by the same key,
so a Create dform gave up waiting for is adopted, not made twice. An S3
credential has nothing of the program's to be found by (the API makes
its access key): a Create sent again is answered by the one this
provider process made under its key, once the API has answered it; one
whose answer was lost is made again, and the first is left on the user,
unused, for the control panel. A 429 or
5xx answer is sent again with backoff (`[providers.ovh] retries`,
`backoff`).

## Not done

Managed Kubernetes, databases, load balancers, floating IPs, volume
snapshots and backups, fixed IPv6 or public addresses for an instance,
and multi-attach volumes are not in this provider yet.

## API behaviour observed

What the live API was seen to do that its schema does not say, each with
the call, when it was seen, and what the provider does about it. The
fake plays each, and `crates/dform-provider-ovh/tests/quirks.rs` holds
the provider to it.

- A private port names its network by the network's OVH id, not the
  OpenStack id it was attached by. `GET
  /cloud/project/{p}/instance/{id}/interface` answers `networkId:
  "pn-…_0"` for a port that `POST …/interface {networkId}` made with the
  network's OpenStack id in the region (the instance create's
  `networks[].networkId` takes the OpenStack id too). Seen 2026-10-12.
  The provider maps both ids to the network when it reads an instance's
  ports, and matches either when it looks for a port before attaching or
  detaching one.
- A call the API takes no body for refuses one, `{}` included: `POST
  /cloud/project/{p}/user/{id}/s3Credentials`, `POST
  …/s3Credentials/{access}/secret` and `POST
  /domain/zone/{zone}/refresh` answer 400 "You provided an input body
  while none was expected". Seen 2026-10-12. The provider sends no body
  there: no `Content-Type`, and the empty body signed.
- An instance's `ipAddresses` lags an attach: the new port's address is
  missing from `GET /cloud/project/{p}/instance/{id}` for a while, while
  `GET …/interface` lists the port at once. Seen 2026-10-12..14. The
  provider reads an instance's `networks`, `private_ips`, `private_ip`
  and `public_ip` from its ports, and from `ipAddresses` only where
  the ports carry no address.
- An attach on a network the instance has a port on already makes
  another port: the lab's instance has three on one network, from the
  attach loop the first two fixes ended. Seen by 2026-10-15. The
  provider lists the ports before it attaches and attaches only on a
  network it has none on, keeps the port it attached by id (`ports`),
  and detaches the others on a network only where it knows its own
  (above, the instance's networks).

## Shapes not yet observed

The fake answers from recordings in
`crates/dform-provider-ovh/src/fake/cassettes/`, one per answer model,
each naming the calls that answer it. Every success answer has the
fields of the API's published model (eu.api.ovh.com/1.0, read
2026-10-15), with made-up, redacted values; none is yet a recorded live
answer. A session on the real API should record these, redact them, and
replace the cassettes' answers:

- the OAuth2 token answer (`POST /auth/oauth2/token`), which is not in
  the 1.0 schema: its fields are RFC 6749's;
- every refusal's body but the one above: the fake answers `{"message":
  ...}` with texts of its own (a 404's "X does not exist", a 400 for a
  wrong flavor or image, a smaller flavor, a network not in the region
  or without a subnet there, a used VLAN (409), no vRack, a subnet
  without `start` or `end`, a network that still has subnets, a user not
  ready, a container not empty (409), a volume in the wrong status), and
  the signed-call refusals ("Invalid signature", "Query out of time",
  `INVALID_CREDENTIAL`, a 401 "Invalid token"); OVH's may carry `class`,
  `errorCode` and `httpCode`;
- a void call's answer (`PUT`, `DELETE`, the refresh): `null`;
- `…/interface`: the public port's `networkId`, whether the `POST`'s
  answer names the network as the `GET` does and carries the port's
  `id` (the provider keeps it as the instance's own port), whether a
  new port's `fixedIps` is filled at once, and the `POST`'s `ip` making
  the port with that address;
- `…/instance`: the create's `networks[].ip` making the port with that
  address, and its refusals of one in no subnet and of one taken (the
  fake's 400 and 409); whether the list embeds `flavor`, `image` and `sshKey`,
  an address's `networkId` in `ipAddresses`, `ipAddresses` empty while
  `BUILD`, a resize confirmed by OVH (`RESIZE` then `ACTIVE`, never
  `VERIFY_RESIZE`), how long `ipAddresses` lags (the fake counts reads,
  `Server::ip_lag`), and a deleted instance answering 404 at once;
- `…/network/public`: its id, name and `vlanId`, and its regions'
  OpenStack ids;
- `…/network/private`: `BUILDING` then `ACTIVE` on a create, every region
  when no `regions` is given, a region added `ACTIVE` at once, a delete
  `DELETING` then gone;
- `…/subnet`: the gateway at the range's first host, `start` and `end`
  required, a region where the network is not `ACTIVE` refused;
- `…/volume`: the statuses on the way (`creating`, `attaching`, `in-use`,
  `detaching`, `extending`, `deleting`), an attach refused unless
  `available`, a delete refused while `in-use`, a volume detached when
  its instance is deleted;
- `…/user`: `creating` then `ok`, an S3 credential refused until `ok`,
  the role ids, `PUT …/role` answering the password as the create does;
- `…/region/{r}/storage`: the owner when none is given, versioning that
  cannot go back to `disabled`, a container with objects not deleted;
- `/domain/zone/{zone}/record`: a record made without a ttl answering 0,
  `PUT` taking `subDomain`, `target` and `ttl` only; a zone the account
  does not host answering 404;
- `/cloud/project/{p}/vrack`: a project without one answering 404.

## Testing

`cargo test --test main -- provider_ovh::` runs it against a fake OVH API
(`dform_provider_ovh::fake`, with a DNS resolver beside it), which
answers from the recordings above and plays what the live API was seen
to do; `cargo test -p dform-provider-ovh --test quirks` checks each of
those, and `--test lists` counts what a session asks of it. `OVH_INTEGRATION=1
OVH_CLOUD_PROJECT_SERVICE=<project> cargo test --test main --
provider_ovh::the_real_account` lists the regions of the real account the machine's
configuration names, and changes nothing.
