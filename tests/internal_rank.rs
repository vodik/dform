//! A provider's own value is `@internal`, below `@default` (WORK.org,
//! 2026-10-17): k8s's `revisionHistoryLimit: 10`, read back from the
//! world, yields to a policy's `@default` without a conflict, and its site
//! is `(provider default)` where a site is said from `-v`.

use crate::common;
use common::{Run, Scratch};

const DEPLOYMENT: &str = "\n\nuse k8s\n\n\
     resource k8s.deployment web {\n  \
     metadata.name = \"web\"\n  \
     spec.selector.matchLabels = { app = \"web\" }\n  \
     spec.template.metadata.labels = { app = \"web\" }\n  \
     spec.template.spec.containers = [{ name = \"api\", image = \"api:1\" }]\n\
     }\n";

/// The policy of the user's paste: every deployment keeps one revision,
/// at `@default`.
const POLICY: &str = "\nset w.spec.revisionHistoryLimit = 1 @default where w in k8s.deployment\n";

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s
}

fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend(args);
    s.run(&all)
}

/// The deployment applied without the policy (the server defaults its
/// history to 10), then the policy: one update, 10 to 1, no conflict.
#[test]
fn a_default_overrules_the_providers_value_without_a_conflict() {
    let s = project("internal-overruled");
    s.write("main.df", DEPLOYMENT);
    dev(&s, &["apply", "--yes", "main.df"]).success();
    s.write("main.df", &format!("{DEPLOYMENT}{POLICY}"));
    let r = dev(&s, &["plan", "main.df"]).success();
    assert!(!r.stdout.contains("conflict"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("spec.revisionHistoryLimit = 10 → 1  main.df:12\n"),
        "{}",
        r.stdout
    );
    // From `-v`, what it won over: the provider's value, not a line.
    let r = dev(&s, &["plan", "-v", "main.df"]).success();
    assert!(
        r.stdout.contains(
            "spec.revisionHistoryLimit = 10 → 1  main.df:12  @default over (provider default)\n"
        ),
        "{}",
        r.stdout
    );
}

/// A create: the policy's value is the document's, not a null the
/// provider was to fill.
#[test]
fn a_default_is_a_creates_value() {
    let s = project("internal-create");
    s.write("main.df", &format!("{DEPLOYMENT}{POLICY}"));
    let r = dev(&s, &["plan", "main.df"]).success();
    assert!(
        r.stdout
            .contains("spec.revisionHistoryLimit = 1  main.df:12\n"),
        "{}",
        r.stdout
    );
}

/// `why` says a value the provider gave as such, and what beat it.
#[test]
fn why_says_a_providers_value_is_its_default() {
    let s = project("internal-why");
    s.write("main.df", DEPLOYMENT);
    dev(&s, &["apply", "--yes", "main.df"]).success();
    let r = dev(&s, &["why", "web.spec.revisionHistoryLimit", "main.df"]).success();
    assert_eq!(
        r.stdout,
        "k8s.deployment web.spec.revisionHistoryLimit = 10  (provider default)\n"
    );
    s.write("main.df", &format!("{DEPLOYMENT}{POLICY}"));
    let r = dev(&s, &["why", "web.spec.revisionHistoryLimit", "main.df"]).success();
    assert!(
        r.stdout.contains("  over 10       (provider default)\n"),
        "{}",
        r.stdout
    );
}
