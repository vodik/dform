//! A list change prints as an edit script, not index shifts (notes.org
//! item 82, the 2026-10-17 decision): under the attribute, the list
//! whole, one element a line, unchanged plain, `+`/`-` an insert or a
//! remove, `~ before → after` a change in place, a long unchanged run
//! elided; a keyed list still by its key (R-158), unaffected.

use crate::common;
use common::Scratch;

const ARGS: [&str; 13] = [
    "--entrypoints.web.address=:80",
    "--entrypoints.websecure.address=:443",
    "--providers.kubernetescrd",
    "--log.level=INFO",
    "--entrypoints.web.http.redirections.entrypoint.to=websecure",
    "--entrypoints.web.http.redirections.entrypoint.scheme=https",
    "--metrics.prometheus",
    "--certificatesresolvers.letsencrypt.acme.email=admin@vodik.xyz",
    "--certificatesresolvers.letsencrypt.acme.storage=/acme/acme.json",
    "--certificatesresolvers.letsencrypt.acme.tlschallenge=true",
    "--ping",
    "--accesslog",
    "--api.dashboard",
];

fn deployment(args: &[&str]) -> String {
    let args = args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
use k8s
resource k8s.deployment traefik {{
  metadata.name = "traefik"
  spec.selector.matchLabels = {{ app = "traefik" }}
  spec.template.spec.containers = [{{
    name = "traefik",
    image = "traefik:3.0",
    args = [{args}],
  }}]
}}
"#
    )
}

fn plan(s: &Scratch) -> common::Run {
    s.run(&common::on(
        "p.df",
        &["--provider", "k8s", "--world", "w.json"],
        &["plan"],
    ))
}

/// Thirteen args with one inserted at index 7 (notes.org item 82, the
/// traefik case): the whole list prints under `args`, the insert one `+`
/// line between its neighbours, the rest elided (over about twelve
/// elements) down to each change's one neighbour a side.
#[test]
fn one_element_inserted_prints_one_plus_line_between_its_neighbours() {
    let s = Scratch::new("list-diffs-insert");
    s.write("p.df", &deployment(&ARGS));
    s.run(&common::on(
        "p.df",
        &["--provider", "k8s", "--world", "w.json"],
        &["apply"],
    ))
    .success();

    let mut after = ARGS.to_vec();
    after.insert(7, "--entrypoints.ssh.address=:2222");
    s.write("p.df", &deployment(&after));
    let r = plan(&s).success();

    assert!(
        !r.stdout.contains("args[7]") && !r.stdout.contains("args[8]"),
        "the old per-index form should be gone:\n{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("      spec.template.spec.containers[name=traefik].args = [\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("\n        + \"--entrypoints.ssh.address=:2222\"\n"),
        "{}",
        r.stdout
    );
    // One neighbour each side of the insert, the rest elided (13 kept
    // elements, over the ~12 threshold, around one insert: both runs
    // longer than their one needed neighbour).
    assert!(
        r.stdout.contains("… 6 unchanged …"),
        "the long unchanged runs on both sides of the insert should elide:\n{}",
        r.stdout
    );
    assert!(r.stdout.contains("      ]\n"), "{}", r.stdout);
}

/// Two elements changed in place (same count on both sides of the gap):
/// two `~ before → after` lines, not four `+`/`-` lines.
#[test]
fn two_elements_changed_in_place_print_two_tilde_lines() {
    let s = Scratch::new("list-diffs-change");
    s.write("p.df", &deployment(&ARGS));
    s.run(&common::on(
        "p.df",
        &["--provider", "k8s", "--world", "w.json"],
        &["apply"],
    ))
    .success();

    let mut after = ARGS.to_vec();
    after[4] = "--entrypoints.web.address=:8080";
    after[5] = "--entrypoints.websecure.address=:8443";
    s.write("p.df", &deployment(&after));
    let r = plan(&s).success();

    assert!(
        !r.stdout.contains("args[4]") && !r.stdout.contains("args[5]"),
        "the old per-index form should be gone:\n{}",
        r.stdout
    );
    let tildes = r
        .stdout
        .lines()
        .filter(|l| l.trim_start().starts_with("~ \""))
        .count();
    assert_eq!(tildes, 2, "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "~ \"--entrypoints.web.http.redirections.entrypoint.to=websecure\" → \"--entrypoints.web.address=:8080\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "~ \"--entrypoints.web.http.redirections.entrypoint.scheme=https\" → \"--entrypoints.websecure.address=:8443\"\n"
        ),
        "{}",
        r.stdout
    );
}

/// A keyed list (R-158) is unaffected: still by its key, not a
/// whole-list block.
#[test]
fn a_keyed_list_still_prints_by_its_key() {
    let s = Scratch::new("list-diffs-keyed");
    s.write(
        "p.df",
        r#"
use k8s
resource k8s.deployment api {
  metadata.name = "api"
  spec.selector.matchLabels = { app = "api" }
  spec.template.spec.containers = [
    { name = "app", image = "api:1" },
    { name = "sidecar", image = "envoy:1" },
  ]
}
"#,
    );
    s.run(&common::on(
        "p.df",
        &["--provider", "k8s", "--world", "w.json"],
        &["apply"],
    ))
    .success();
    s.write("p.df", &s.read("p.df").replace("envoy:1", "envoy:2"));
    let r = plan(&s).success();
    assert!(
        r.stdout.contains(
            r#"spec.template.spec.containers[name=sidecar].image = "envoy:1" → "envoy:2""#
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains(" = [\n"), "{}", r.stdout);
}
