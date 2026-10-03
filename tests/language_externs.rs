//! Externs with binding patterns: asked on demand once their `+` arguments
//! are ground, of the `file` provider (a loader, `json(p)`, among them) or
//! the mock (`externs.df`), recorded in the plan file; nothing keeps them
//! (`memo.first` does: tests/memo.rs).

mod common;
use common::{Scratch, repo};

const P: &str = r#"edition 2026
provider file
extern kv.password(+name, -value)
extern kv.token(+name, -value)
dash("dash.json")
resource mon.dashboard main {
  json = d
  note = t
} where dash(p), d = json(p), file.text("note.txt", t)
resource db.user app {
  password = pw
  token = tk
} where {
    kv.password("app", pw)
    kv.token("app", tk)
  }
"#;

fn scratch() -> Scratch {
    let s = Scratch::project("lang-externs");
    s.write("p.df", P);
    s.write("dash.json", r#"{"title": "pngu", "panels": [1, 2]}"#);
    s.write("note.txt", "hello");
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_provider(mon.dashboard, \"fakecloud\")\ntype_provider(db.user, \"fakecloud\")\n"),
    );
    answers(&s, "first");
    s
}

/// What the mock answers: one row per name, and one for another name that
/// no rule asks for.
fn answers(s: &Scratch, v: &str) {
    s.write(
        "providers/fake/externs.df",
        &format!(
            "edition 2026\nkv.password(\"app\", \"pw-{v}\")\nkv.password(\"other\", \"x\")\nkv.token(\"app\", \"tk-{v}\")\n"
        ),
    );
}

#[test]
fn externs_answer_on_demand() {
    let s = scratch();
    let r = s.run(&["plan", "p.df"]).success();
    for line in [
        "json.panels[0] = 1",
        "json.title = \"pngu\"",
        "note = \"hello\"",
        "password = \"pw-first\"",
        "token = \"tk-first\"",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
    let q = s.run(&["query", "kv.password", "p.df"]).success();
    assert_eq!(
        q.stdout.lines().count(),
        2,
        "only the demanded call: {}",
        q.stdout
    );
}

#[test]
fn a_missing_file_is_an_error_naming_the_call() {
    let s = scratch();
    std::fs::remove_file(s.path("note.txt")).unwrap();
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("extern file.text(\"note.txt\")") && r.stderr.contains("read note.txt"),
        "{}",
        r.stderr
    );
}

/// An extern is asked every run: nothing keeps its answer.
#[test]
fn an_answer_is_asked_again() {
    let s = scratch();
    s.run(&["apply", "p.df"]).success();
    assert!(!s.read("dform.state/p/state.json").contains("pw-first"));
    answers(&s, "second");
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("password: \"pw-first\" -> \"pw-second\""),
        "{}",
        r.stdout
    );
}

/// `persist` is gone (R-60): the error names `memo.first`.
#[test]
fn persist_is_an_error_naming_memo_first() {
    let s = scratch();
    s.write(
        "p.df",
        &P.replace(
            "extern kv.password(+name, -value)",
            "extern kv.password(+name, -value) persist",
        ),
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("extern kv.password: `persist` is gone")
            && r.stderr.contains("memo.first(KEY, CANDIDATE, VALUE)"),
        "{}",
        r.stderr
    );
}

/// The plan file records the answers the plan read; apply asks none of
/// them again, so it applies what the plan showed.
#[test]
fn the_plan_file_records_the_answers() {
    let s = scratch();
    s.run(&["plan", "--out", "plan.json", "p.df"]).success();
    assert!(s.read("plan.json").contains("tk-first"));
    answers(&s, "second");
    s.run(&["apply", "plan.json"]).success();
    let world = s.read("dform.state/p/remote.json");
    assert!(
        world.contains("tk-first") && !world.contains("tk-second"),
        "{world}"
    );
}
