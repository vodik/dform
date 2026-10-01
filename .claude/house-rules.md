# dform house rules for agents

- Semantics come from proposals/E-synthesis.org as revised by proposals/F-revision.org
  section 3. When a ticket and E/F disagree, the ticket wins; say so in the report.
- Do not optimize inside a semantics ticket.
- Keep `cargo run -q -- -C examples/demo plan` and
  `cargo run -q -- -C examples/pngu plan pngu env=prod` working at every commit.
  They are gates.
- Programs follow docs/layout.md: one project per directory with a dform.toml.
- Every behaviour change ships with a test that fails with the change reverted, and
  the report says you checked.
- Snapshot tests live under tests/golden/; accept with the documented command only.
- New CLI flags are documented in README.md in the same commit.
- No new dependencies without naming them in the report (insta, logos, ariadne are pre-approved).
- Delete code the ticket says to delete; do not leave the old path behind a flag.
- Rust 2024 edition, rustfmt defaults, no clippy warnings on files you touch.
- Error messages name the resource address and attribute path; never just "conflict".
- Nothing is added to crates/dform-grpc (DESIGN.org R-13): it is the frozen native bridge until the wasm bridge passes `provider check`.
- Decisions of the 2026-10-01 review are DESIGN.org "Review of 2026-10-01: decisions" (R-1 to R-14); a ticket that cites one follows it.
