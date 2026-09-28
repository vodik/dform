//! Externs with binding patterns: asked on demand once their `+` arguments
//! are ground, of the `file` provider or the mock (`externs.df`), recorded
//! in the plan file, and with `persist` kept in state.

mod common;
use common::{Scratch, repo};

const P: &str = r#"edition 2026.
extern file.json(+path, -value).
extern file.text(+path, -value).
extern random.password(+name, -value) persist.
extern random.token(+name, -value).
dash("dash.json").
resource mon.dashboard main { json = D, note = T } :- dash(P), file.json(P, D), file.text("note.txt", T).
resource db.user app { password = Pw, token = Tk } :-
  random.password("app", Pw),
  random.token("app", Tk).
"#;

fn scratch() -> Scratch {
    let s = Scratch::new("lang-externs");
    s.write("p.df", P);
    s.write("dash.json", r#"{"title": "pngu", "panels": [1, 2]}"#);
    s.write("note.txt", "hello");
    s.write(
        "providers/fake/schema.df",
        &std::fs::read_to_string(repo().join("providers/fake/schema.df")).unwrap(),
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
            "edition 2026.\nrandom.password(\"app\", \"pw-{v}\").\nrandom.password(\"other\", \"x\").\n\
             random.token(\"app\", \"tk-{v}\").\n"
        ),
    );
}

#[test]
fn externs_answer_on_demand() {
    let s = scratch();
    let r = s.run(&["--file", "p.df", "plan"]).success();
    for line in [
        "json.panels[0] = 1",
        "json.title = \"pngu\"",
        "note = \"hello\"",
        "password = \"pw-first\"",
        "token = \"tk-first\"",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
    let q = s
        .run(&["--file", "p.df", "query", "random.password"])
        .success();
    assert!(
        q.stdout.contains("matches: 1"),
        "only the demanded call: {}",
        q.stdout
    );
}

#[test]
fn a_missing_file_is_an_error_naming_the_call() {
    let s = scratch();
    std::fs::remove_file(s.path("note.txt")).unwrap();
    let r = s.run(&["--file", "p.df", "plan"]).failure();
    assert!(
        r.stderr.contains("extern file.text(\"note.txt\")") && r.stderr.contains("read note.txt"),
        "{}",
        r.stderr
    );
}

/// A `persist` extern's answer is kept in state and not asked again; a
/// plain extern is asked every run.
#[test]
fn a_persisted_answer_stays() {
    let s = scratch();
    s.run(&["--file", "p.df", "apply"]).success();
    assert!(s.read(".dform/p/state.json").contains("pw-first"));
    assert!(!s.read(".dform/p/state.json").contains("tk-first"));
    answers(&s, "second");
    let r = s.run(&["--file", "p.df", "plan"]).success();
    assert!(
        r.stdout.contains("token: \"tk-first\" -> \"tk-second\""),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("pw-second"), "{}", r.stdout);
}

/// `dform taint` forgets one persisted answer: the next plan asks the
/// provider again, and only for that call.
#[test]
fn taint_forgets_a_persisted_answer() {
    let s = scratch();
    s.run(&["--file", "p.df", "apply"]).success();
    answers(&s, "second");
    let r = s
        .run(&["--file", "p.df", "taint", "p", "random.password", "other"])
        .failure();
    assert!(
        r.stderr
            .contains("taint random.password(other): stack p has no persisted answer for it"),
        "{}",
        r.stderr
    );
    let r = s
        .run(&["--file", "p.df", "taint", "p", "random.password", "app"])
        .success();
    assert_eq!(
        r.stdout,
        "tainted random.password(app) of stack p: the next plan asks again\n"
    );
    assert!(!s.read(".dform/p/state.json").contains("pw-first"));
    let r = s.run(&["--file", "p.df", "plan"]).success();
    assert!(
        r.stdout.contains("password: \"pw-first\" -> \"pw-second\""),
        "{}",
        r.stdout
    );
}

/// The plan file records the answers the plan read; apply asks none of
/// them again, so it applies what the plan showed.
#[test]
fn the_plan_file_records_the_answers() {
    let s = scratch();
    s.run(&["--file", "p.df", "plan", "--out", "plan.json"])
        .success();
    assert!(s.read("plan.json").contains("tk-first"));
    answers(&s, "second");
    s.run(&["apply", "plan.json"]).success();
    let world = s.read(".dform/p/remote.json");
    assert!(
        world.contains("tk-first") && !world.contains("tk-second"),
        "{world}"
    );
}
