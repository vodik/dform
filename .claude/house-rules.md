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
- Messages (R-109): an error says what happened with its origin, the rule in one clause, and a help that is the fix at that site, computed from it (the names, the line, the nearest name in scope), never a menu; one help string serves one kind of error (tests/syntax.rs, tests/language_secrets.rs). No compiler word (extern, atom, stratum, lattice, null, scoped, head, body, lowering) reaches a user unless the program wrote it: say a clause or "after `where`", a relation, a data source; every Scratch run checks its stderr (tests/common). An address prints through `report::address` (the plan's form), never `T["A"]` outside state files, `--json` and `-q`; a deployment is named as the user writes it.

- A consolidation pass also reviews the code the way a veteran Rust developer would, file by file, and fixes what it finds: methods on the type instead of free functions that take it first; small functions, one job each; a utility struct for state or an accumulator that several functions thread through, with an `into_result`-style finish; moves instead of borrow-then-clone where the value is consumed; modules organized by idea with nothing in the wrong file; a builder where a constructor has grown optional parts and it improves the call sites; `From`/`Into` for an infallible conversion and `TryFrom`/`TryInto` for a fallible one, in place of hand-rolled `to_x`/`parse_x`/`as_x` functions, so call sites read `x.into()` and `T::try_from(y)?`. It reviews the tests the same way: a test earns its place by checking a promise someone relies on, in the cheapest place that promise can be checked. Delete a test that proves what another already proves, that races a clock or a sleep, that drives a process or a pty for logic a function call would check, or that exists so a feature "has a test". Fewer, sharper tests over coverage. Each fix is its own commit; no behaviour change.

- New syntax or a new feature is generalized before it is built: ask where else the same notation or idea already appears or could, and give it one meaning in every position (a literal's spread and a pattern's rest are one `..`; a let with parameters, a provider table and a std function are one kind of relation with a mode; `in` is membership for every ordered type). A feature that fits only its first use is not ready; a feature that removes a special case is. Say in the ticket what it generalizes and what it was deliberately not extended to, and why.
