# Golden tests

`tests/golden.rs` runs `dform plan` and `dform strata` for a table of
programs (the example programs, the adversarial stratification cases,
the k8s and aws-mock demos) and diffs the output against a snapshot file
here: `tests/golden/<program>/<case>.<plan|strata>.txt`.

Each snapshot is a small transcript, not just stdout, so a change in exit
status or stderr is caught too:

```
exit: ok
-- stdout --
...
-- stderr --
...
```

A case that is expected to fail (a rejected stratification, a blocked
constraint) still gets a snapshot: `exit: error` plus whatever it printed.
That's the point of a golden test — it pins current behavior, not a
judgment about what's correct.

## Accepting a change

```bash
UPDATE_GOLDEN=1 cargo test --test golden -- --test-threads=1
```

`--test-threads=1` matters when regenerating: one test writes the
snapshots, another reads one back as a sanity check, and they must not
race. Ordinary (non-updating) runs are read-only and don't need it.

Review the diff (`git diff tests/golden/`) before committing — this
records that the new output is intentional, not a regression. The
engine's semantics work (nulls, phases, Z-set planning) is expected to
change several of these snapshots; re-accept them as it lands.
