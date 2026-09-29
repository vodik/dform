# tour
Start here: one stack file, stacks/tour.df, read top to bottom as a tutorial.
A dform program is facts and rules; the plan is their difference from the world, unknowns shown.
Policy is more rules, checked in every plan, and `dform why` explains any value.
Each section says the command to run and what it prints; on the fake provider, no credentials.
```bash
dform plan                                     # 11 creates, the unknowns marked ?
dform apply                                    # asks, then one tick
dform plan tour env=prod                       # prod is a deployment of its own
dform plan --set public_db=true                # refused by a deny, on purpose
dform why 'net.vpc["main"].tags.team'          # the tag's rule, file and line
```
