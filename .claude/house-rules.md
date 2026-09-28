# dform house rules for agents

- Semantics come from proposals/E-synthesis.org as revised by proposals/F-revision.org
  section 3. When a ticket and E/F disagree, the ticket wins; say so in the report.
- Naive evaluation is fine. Do not optimize inside a semantics ticket.
- Keep `cargo run -- plan` (dform.df) and `cargo run -- --file pngu.df plan --set env=prod`
  working at every commit. They are gates.
- Every behaviour change ships with a test that fails with the change reverted, and
  the report says you checked.
- Snapshot tests live under tests/golden/; accept with the documented command only.
- New CLI flags are documented in README.md in the same commit.
- No new dependencies without naming them in the report (insta, logos, ariadne are pre-approved).
- Delete code the ticket says to delete; do not leave the old path behind a flag.
- Rust 2024 edition, rustfmt defaults, no clippy warnings on files you touch.
- Error messages name the resource address and attribute path; never just "conflict".
