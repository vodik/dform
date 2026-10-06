# tour
Start here: stacks/tour.df is a tutorial, read top to bottom, each section one idea built on the last.
It starts where Terraform does (resources, references, for_each, environments, modules and components) and goes on
to what Terraform cannot say: policy as rules in the program, `why` for any value, recursive rules,
and resources named by values only apply learns. On a fake cloud: no credentials.
```bash
dform plan                                  # everything is a create, each with its file and line
dform apply                                 # asks, then asks again at tick 2
dform plan tour env=prod                    # prod is a deployment of its own
dform plan --set public_db=true             # refused by a deny, on purpose
dform why 'net.vpc main.tags.team'          # the tag's rule, file and line
dform query 'reaches("blue", x)'            # a recursive rule, asked
```
