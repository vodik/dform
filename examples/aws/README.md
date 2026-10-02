# aws
A small web stack on the Terraform-shaped mock AWS provider; the world is a JSON file.
```bash
dform plan    # 7 creates
dform apply   # one tick
```
Optional+Computed attributes are nulls until Apply (`?aws.instance["web"].availability_zone`); keyless
ingress rules compare as a set. Features: a provider from `dform.toml` (`aws-mock`), refs, open nulls.
