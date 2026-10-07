//! `memo.first(+key, +candidate, -value)` (R-60): the first candidate given
//! for a key is kept in state by the apply and is the value on every later
//! run; `dform secrets rotate DEPLOYMENT KEY` forgets it (R-161). A secret candidate is
//! kept sealed with the stack's key, never in the clear. `time.now()` is
//! the `time` provider's extern, read again every run.

mod common;
use common::{Run, Scratch, dform, repo, yes};

/// `dform ARGS` in `s` with the environment `env` (`RANDOM_MASTER` unset
/// unless given).
fn run(s: &Scratch, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env_remove("RANDOM_MASTER");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

fn project(name: &str, program: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", program);
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_provider(db.user, \"fakecloud\")\n\
               type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n"),
    );
    s
}

/// What the mock answers `kv.password("app")` with.
fn answers(s: &Scratch, v: &str) {
    s.write(
        "providers/fake/externs.df",
        &format!("\nkv.password(\"app\", \"pw-{v}\")\n"),
    );
}

const PLAIN: &str = r#"
use fake
extern kv.password(+name, -value)
resource db.user app {
  password = pw
} where {
    kv.password("app", candidate)
    memo.first("app-pw", candidate, pw)
  }
"#;

/// A memo survives a re-plan whatever the candidate becomes, and is gone
/// after `secrets rotate D KEY`: the next run keeps its candidate.
#[test]
fn a_memo_survives_a_replan_and_is_gone_after_a_rotation() {
    let s = project("memo-plain", PLAIN);
    answers(&s, "first");
    let now = [("DFORM_TEST_NOW", "2026-10-02T09:00:00Z")];
    run(&s, &now, &["apply", "p.df"]).success();
    let state = s.read("dform.state/p/state.json");
    assert!(
        state.contains("\"app-pw\"")
            && state.contains("pw-first")
            && state.contains("2026-10-02T09:00:00Z"),
        "{state}"
    );
    answers(&s, "second");
    let r = run(&s, &[], &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    // `why` says the value is a memo's and when it was kept.
    let r = run(
        &s,
        &[],
        &["why", "--tree", "db.user[\"app\"].password", "p.df"],
    )
    .success();
    assert!(
        r.stdout.contains("memo, first kept 2026-10-02T09:00:00Z"),
        "{}",
        r.stdout
    );

    let r = run(&s, &[], &["secrets", "rotate", "p", "other"]).failure();
    assert!(
        r.stderr
            .contains("secrets rotate other: p has no secret other (its keys: none)"),
        "{}",
        r.stderr
    );
    let r = run(&s, &[], &["secrets", "rotate", "p", "app-pw"]).success();
    assert!(
        r.stdout.starts_with(
            "rotating app-pw of p (memo): generation 1 -> 2\n  forgot what memo.first keeps: \
             the next apply keeps its candidate\nrotated app-pw of p: generation 2, by "
        ),
        "{}",
        r.stdout
    );
    assert!(!s.read("dform.state/p/state.json").contains("pw-first"));
    let r = run(&s, &[], &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("password = \"pw-first\" → \"pw-second\""),
        "{}",
        r.stdout
    );
}

/// Within a run the first call of a key answers every later one: two
/// sites with two candidates agree, and a plan keeps nothing.
#[test]
fn two_sites_of_a_key_agree_and_a_plan_keeps_nothing() {
    let s = project(
        "memo-sites",
        r#"
use fake
resource db.user a {
  password = memo.first("pw", "from-a")
}
resource db.user b {
  password = memo.first("pw", "from-b")
}
"#,
    );
    let r = run(&s, &[], &["plan", "p.df"]).success();
    let a = r.stdout.matches("password = \"from-a\"").count();
    let b = r.stdout.matches("password = \"from-b\"").count();
    assert!(a + b == 2 && (a == 0 || b == 0), "{}", r.stdout);
    assert!(
        !s.path("dform.state/p/state.json").exists()
            || !s.read("dform.state/p/state.json").contains("\"memo\""),
    );
}

const SECRET: &str = r#"
use fake
resource db.secret v {
  password = pw
} where memo.first("db-pw", random.password("db-pw"), pw)
"#;

/// A secret memo (`memo.first(K, random.password(K), V)`) is kept sealed:
/// no byte of it is under dform.state, in a plan file or in any output; a
/// new master derives another candidate, and the kept one stays until the
/// memo is rotated.
#[test]
fn a_secret_memo_is_kept_sealed_never_in_the_clear() {
    let s = project("memo-secret", SECRET);
    let first = [("RANDOM_MASTER", "first-master")];
    let r = run(&s, &first, &["apply", "p.df"]).success();
    let world: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/p/remote.json")).unwrap();
    let pw = world["resources"]["db.secret::v"]["attrs"]["password"]
        .as_str()
        .unwrap_or_else(|| panic!("{world}"))
        .to_string();
    assert_eq!(pw.len(), 32, "{pw}");
    let mut outputs = vec![r.stdout, r.stderr];

    // Another master: the candidate changes, the kept value does not.
    let second = [("RANDOM_MASTER", "second-master")];
    let r = run(
        &s,
        &second,
        &["plan", "--new-master", "--out", "plan.json", "p.df"],
    )
    .success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    outputs.extend([r.stdout, r.stderr, s.read("plan.json")]);
    for args in [
        &["why", "db.secret[\"v\"].password", "p.df"][..],
        &["query", "memo.first", "p.df"][..],
    ] {
        let r = run(&s, &second, args).success();
        outputs.extend([r.stdout, r.stderr]);
    }
    let state = s.read("dform.state/p/state.json");
    let kept: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert!(
        kept["memo"]["db-pw"]["sealed"].is_string() && kept["memo"]["db-pw"]["value"].is_null(),
        "{state}"
    );
    let mut files = vec![("state.json".to_string(), state)];
    for e in std::fs::read_dir(s.path("dform.state/p")).unwrap() {
        let p = e.unwrap().path();
        if p.file_name().is_some_and(|n| n != "remote.json")
            && let Ok(text) = std::fs::read(&p)
        {
            files.push((
                p.display().to_string(),
                String::from_utf8_lossy(&text).into(),
            ));
        }
    }
    for (what, text) in files.iter().chain(
        outputs
            .iter()
            .map(|o| ("output".to_string(), o.clone()))
            .collect::<Vec<_>>()
            .iter(),
    ) {
        assert!(!text.contains(&pw), "{what}:\n{text}");
    }

    // Rotated, the next apply keeps the new master's candidate, which no
    // output of the runs before showed either.
    let r = run(&s, &[], &["secrets", "rotate", "p", "db-pw"]).success();
    assert!(!r.stdout.contains(&pw), "{}", r.stdout);
    let r = run(&s, &second, &["plan", "--new-master", "p.df"]).success();
    assert!(r.summary().contains("1 update"), "{}", r.stdout);
    assert!(!r.stdout.contains(&pw), "{}", r.stdout);
    run(&s, &second, &["apply", "--new-master", "p.df"]).success();
    let world: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/p/remote.json")).unwrap();
    let pw2 = world["resources"]["db.secret::v"]["attrs"]["password"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(pw, pw2);
    for o in &outputs {
        assert!(!o.contains(&pw2), "{o}");
    }
    assert!(!s.read("dform.state/p/state.json").contains(&pw2));
}

/// The rotation idiom: a creation time kept once, compared with the time
/// read every run.
#[test]
fn a_kept_creation_time_drives_a_rotation() {
    let s = project(
        "memo-rotate",
        r#"
use fake
use time
resource db.user app {
  password = "x"
}
warn "rotate the password" where {
  memo.first("db-created", time.now(), created)
  created + 30d < time.now()
}
"#,
    );
    let at = |t: &'static str| [("DFORM_TEST_NOW", t)];
    let r = run(&s, &at("2026-10-02T09:00:00Z"), &["apply", "p.df"]).success();
    assert!(!r.stdout.contains("rotate"), "{}", r.stdout);
    let r = run(&s, &at("2026-10-20T09:00:00Z"), &["plan", "p.df"]).success();
    assert!(!r.stdout.contains("rotate"), "{}", r.stdout);
    let r = run(&s, &at("2026-11-02T09:00:00Z"), &["plan", "p.df"]).success();
    assert!(
        format!("{}{}", r.stdout, r.stderr).contains("rotate the password"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// `time.now()` needs `use time`, like any built-in provider's extern.
#[test]
fn time_now_is_the_time_providers() {
    let s = project(
        "memo-time",
        r#"
use fake
resource db.user app {
  password = time.format(time.now(), "%Y")
}
"#,
    );
    let r = run(&s, &[], &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("time.now is the time provider's: declare `use time`"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &s.read("p.df").replace("use fake\n", "use fake\nuse time\n"),
    );
    let r = run(
        &s,
        &[("DFORM_TEST_NOW", "2031-01-01T00:00:00Z")],
        &["plan", "p.df"],
    )
    .success();
    assert!(r.stdout.contains("password = \"2031\""), "{}", r.stdout);
}

/// A memo first read by an apply with nothing to do is kept: the apply
/// saves state when the memo table changed, not only when the world did.
#[test]
fn a_no_op_apply_keeps_a_memo_it_first_read() {
    let s = project(
        "memo-noop",
        "\nuse fake\nresource db.user a {\n  password = \"same\"\n}\n",
    );
    run(&s, &[], &["apply", "p.df"]).success();
    s.write(
        "p.df",
        "\nuse fake\nresource db.user a {\n  password = memo.first(\"pw\", \"same\")\n}\n",
    );
    let now = [("DFORM_TEST_NOW", "2026-10-03T09:00:00Z")];
    let r = run(&s, &now, &["apply", "p.df"]).success();
    assert!(r.stdout.ends_with("is up to date\n"), "{}", r.stdout);
    let state = s.read("dform.state/p/state.json");
    assert!(
        state.contains("\"pw\"") && state.contains("2026-10-03T09:00:00Z"),
        "{state}"
    );
}
