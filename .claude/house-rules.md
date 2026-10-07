# dform house rules for agents

- Semantics come from proposals/E-synthesis.org as revised by proposals/F-revision.org
  section 3. When a ticket and E/F disagree, the ticket wins; say so in the report.
- Do not optimize inside a semantics ticket.
- Keep `cargo run -q -- -C examples/demo plan` and
  `cargo run -q -- -C examples/pngu plan pngu env=prod` working at every commit.
  They are gates.
- `cargo fmt --all --check` is a gate: the tree stays in rustfmt's form, so no
  branch carries another's formatting hunks.
- Programs follow docs/layout.md: one project per directory with a dform.toml.
- Every behaviour change ships with a test that fails with the change reverted, and
  the report says you checked.
- Snapshot tests live under tests/golden/; accept with the documented command only.
- New CLI flags are documented in docs/reference.md in the same commit; the README is the
  introduction and changes only when the surface it describes does.
- No new dependencies without naming them in the report (insta, logos, ariadne are pre-approved).
- Delete code the ticket says to delete; do not leave the old path behind a flag.
- Rust 2024 edition, rustfmt defaults, no clippy warnings on files you touch.
- Error messages name the resource address and attribute path; never just "conflict".
- crates/dform-grpc and proto/ change additively only, derived from the WIT (DESIGN.org R-13, R-130): wit/dform-provider.wit is the one definition, a call or a field is added there first, and crates/dform-wit/tests/drift.rs keeps the proto equal to it.
- Decisions of the 2026-10-01 review are DESIGN.org "Review of 2026-10-01: decisions" (R-1 to R-14); a ticket that cites one follows it.
- After each fan-out round lands, one consolidation pass (WORK.org "Consolidation pass after each round") runs before the next round starts: duplicated helpers, parallel implementations and copied test utilities are merged to one, with no behaviour change.
- No shell-outs: git, ssh, http and archives are in-process libraries (gix, russh, the HTTP client). The only `Command::new` allowed are the provider launcher and the configured audit sink (`sh -c`), because the operator asked for a command there, and dform running itself (`diff --since`'s `dform __explain`); tests/no_shell_outs.rs holds the list.
