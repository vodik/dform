use super::labels::printed_label;
use super::layout::{Row, layout};
use super::lines::terse;
use super::mask::LONG;
use super::tally::{KINDS, by_kind, changes_text};
use super::*;
use crate::spell;
use serde_json::Value as Json;

/// After R-23: a binding is left out when its value is a whole
/// segment of the address, not a part of one (`n = 1` is not shown
/// by `private-us-east-1b`).
#[test]
fn a_binding_is_hidden_by_a_whole_segment_of_the_address() {
    let with = |b: &[&str]| b.iter().map(|b| b.to_string()).collect::<Vec<_>>();
    let zone = with(&["zone = \"us-east-1b\"", "n = 1"]);
    assert_eq!(
        terse(&zone, "aws.subnet private-us-east-1b").as_deref(),
        Some("with n = 1")
    );
    assert_eq!(terse(&with(&["n = 3"]), "k3s.agent-3"), None);
    assert_eq!(
        terse(&with(&["n = 3"]), "k3s.agent-31"),
        Some("with n = 3".into())
    );
    assert_eq!(terse(&with(&["n = 0"]), "net.subnet a[0]"), None);
}

/// After R-149: a run that refuses prints a deny as the plan's `!`
/// line, its bindings `key = value` in the site column aligned across
/// the lines, a reference by its address, and never the context's
/// JSON; bindings too wide for the column go beneath it.
#[test]
fn a_violation_prints_its_bindings_never_its_json() {
    let r = Redactor::new(&BTreeSet::new(), &Schema::default());
    let vs = [
        r#"image not pinned ctx={"image":"traefik:v3.7","replicas":2}"#.to_string(),
        r#"no owner ctx={"of":"ref(net.vpc,main,)"}"#.to_string(),
        "need the pngu namespace".to_string(),
    ];
    assert_eq!(
        violations(&vs, &r, Style::PLAIN),
        "  ! image not pinned  image = \"traefik:v3.7\", replicas = 2\n  \
             ! no owner          of = net.vpc main\n  \
             ! need the pngu namespace\n"
    );
    assert_eq!(
        violation_line(&vs[0], &r),
        "image not pinned  image = \"traefik:v3.7\", replicas = 2"
    );
    let wide = format!(r#"too wide ctx={{"a":"{}","b":1}}"#, "x".repeat(90));
    let out = violations(&[wide], &r, Style::PLAIN);
    assert!(out.starts_with("  ! too wide\n      a = \"xxx"), "{out}");
    assert!(out.ends_with("\n      b = 1\n"), "{out}");
}

fn a(t: &str, n: &str) -> Address {
    Address {
        typ: t.into(),
        name: n.into(),
    }
}

/// An extern's answer is its call, its inputs never read as a path
/// (`127.0.0.1:22,ubuntu,/etc/k3s.yaml` split at its dots); a
/// provider's extern by its column number.
#[test]
fn an_extern_label_is_its_call() {
    let l = crate::value::null_label("ssh.read", "127.0.0.1:22,ubuntu,/etc/k3s.yaml", "4");
    let call = "ssh.read(\"127.0.0.1:22\", \"ubuntu\", \"/etc/k3s.yaml\")";
    assert_eq!(label(&l), call);
    assert_eq!(attribute_label(&l), call);
    assert_eq!(printed_label(&crate::ir::label(&l)), call);
    assert_eq!(waited(&BTreeSet::from([l])), [call]);
    let l = crate::value::null_label("aws.availability_zone", "available", "2");
    assert_eq!(label(&l), "aws.availability_zone(\"available\")");
    // A resource's attribute stays one.
    let l = crate::value::null_label("db.postgres", "d", "endpoint");
    assert_eq!(attribute_label(&l), "db.postgres d.endpoint");
}

/// R-111, R-112: an address is its type and its path, a copy's scope
/// in front, a local name holding a dot one quoted segment; a
/// reference is the path alone.
#[test]
fn an_address_prints_as_the_source_names_it() {
    assert_eq!(
        address(&a("ovh.ssh_key", "k3s.admin")),
        "ovh.ssh_key k3s.admin"
    );
    assert_eq!(
        address(&a("ovh.domain_record", r#"k3s."k8s-lab.vodik.xyz""#)),
        r#"ovh.domain_record k3s."k8s-lab.vodik.xyz""#
    );
    assert_eq!(address(&a("net.vpc", "main")), "net.vpc main");
    assert_eq!(address(&a("x.thing", "a b")), r#"x.thing "a b""#);
    assert_eq!(
        reference(&a("ovh.instance", "k3s.server"), "public_ip"),
        "k3s.server.public_ip"
    );
    assert_eq!(attribute(&a("input", ""), "db.days"), "input db.days");
    assert_eq!(
        label("ovh.instance/k3s.server#public_ip"),
        "k3s.server.public_ip"
    );
    assert_eq!(label("net.vpc/main#id"), "main");
    assert_eq!(attribute_label("net.vpc/main#id"), "net.vpc main");
    assert_eq!(
        address_text(r#"k8s.job["migrate-v${schema}"]"#),
        r#"k8s.job "migrate-v${schema}""#
    );
    assert_eq!(address_text("k8s.job[?]"), "k8s.job ?");
    assert_eq!(address_text(r#"app["blue"]"#), "app blue");
}

/// Past 60 characters a string elides its middle at the default
/// level, and prints whole from `-v`.
#[test]
fn a_summary_counts_the_kinds_there_are_in_order() {
    assert_eq!(changes_text(0, &[("create", 0)]), "plan: 0 changes");
    assert_eq!(
        changes_text(3, &[("create", 2), ("update", 0), ("delete", 1)]),
        "plan: 3 changes (2 create, 1 delete)"
    );
    assert_eq!(by_kind(std::iter::empty()).len(), KINDS.len());
}

/// A right column too wide for the page folds to nothing, except one
/// the line is about (a deny's wait, R-193), which goes below it.
#[test]
fn a_kept_right_column_never_folds_away() {
    let wide = format!("waits on {}", "x".repeat(WIDTH));
    let rows = [
        Row::plain("  deny \"a\"".into()).with(vec![wide.clone()]),
        Row::plain("  deny \"b\"".into())
            .with(vec![wide.clone()])
            .kept(),
    ];
    let text = layout(&rows, Style::PLAIN);
    assert_eq!(text, format!("  deny \"a\"\n  deny \"b\"\n      {wide}\n"));
}

#[test]
fn an_id_prints_its_first_twelve_characters() {
    assert_eq!(short_id("3e58789c0ffee5150aa"), "3e58789c0ffe");
    assert_eq!(short_id("3e58"), "3e58");
}

#[test]
fn a_long_string_elides_its_middle_by_default() {
    let key = format!("ssh-ed25519 {} simon@framework", "A".repeat(68));
    let v = Shown::Value(Json::String(key.clone()));
    let line = v.said(Why::Line);
    assert_eq!(line.chars().count(), LONG + 2, "{line}");
    assert!(line.starts_with("\"ssh-ed25519 AAAA") && line.ends_with("AA simon@framework\""));
    assert!(line.contains('…'), "{line}");
    assert_eq!(v.said(Why::How), spell::quote(&key));
    let short = Shown::Value(Json::String("b2-7".into()));
    assert_eq!(short.said(Why::Line), "\"b2-7\"");
}

/// No `?`: a value not known yet is the reference it is; a secret is
/// `(sensitive)`, by its label from `-v`.
#[test]
fn an_unknown_is_its_reference_and_a_secret_is_sensitive() {
    let null = Shown::Null {
        label: r#"ovh.instance["k3s.server"].public_ip"#.into(),
        class: "computed".into(),
    };
    assert_eq!(null.said(Why::Line), "k3s.server.public_ip");
    assert_eq!(null.text(), r#"?ovh.instance["k3s.server"].public_ip"#);
    let secret = Shown::Sensitive(Some(r#"db.instance["main"].password"#.into()));
    assert_eq!(secret.said(Why::Line), "(sensitive)");
    assert_eq!(
        secret.said(Why::How),
        "(sensitive db.instance main.password)"
    );
    let r = Shown::Ref {
        addr: a("net.vpc", "main.vpc"),
        value: Json::String("vpc-1".into()),
    };
    assert_eq!(r.said(Why::Line), "main.vpc");
    assert_eq!(r.text(), r#"net.vpc["main.vpc"]"#);
}

/// Colour is a hint: the address bold in its kind's colour, the site
/// column dim; plain, nothing.
#[test]
fn colour_follows_the_kind() {
    let c = Style { color: true };
    assert_eq!(
        c.address(&ActionKind::Create, "net.vpc main"),
        "\x1b[1;32mnet.vpc main\x1b[0m"
    );
    assert_eq!(
        c.address(&ActionKind::Delete, "net.vpc main"),
        "\x1b[1;31mnet.vpc main\x1b[0m"
    );
    assert_eq!(c.paint(Paint::Dim, "p.df:3"), "\x1b[2mp.df:3\x1b[0m");
    assert_eq!(c.paint(Paint::Because, "because"), "\x1b[36mbecause\x1b[0m");
    assert_eq!(Style::PLAIN.address(&ActionKind::Create, "x"), "x");
}

/// A note about a value is dim where a value would be, inside a value
/// too but never inside a string; plain, nothing.
#[test]
fn notes_are_dim() {
    let c = Style { color: true };
    let row =
        r#"  stringData = { "a": (sensitive), "b": "(sensitive)", "c": (sensitive, rotated) }"#;
    assert_eq!(
        c.notes_in(row),
        "  stringData = { \"a\": \x1b[2m(sensitive)\x1b[0m, \"b\": \"(sensitive)\", \
             \"c\": \x1b[2m(sensitive, rotated)\x1b[0m }"
    );
    assert_eq!(Style::PLAIN.notes_in(row), row);
    assert_eq!(c.note(KEPT), "\x1b[2m(bootstrap): kept\x1b[0m");
    assert_eq!(Style::PLAIN.note(KEPT), KEPT);
    let secret = Shown::Sensitive(None);
    assert_eq!(c.said(&secret, Why::Line), "\x1b[2m(sensitive)\x1b[0m");
}

/// A kept value's note (R-218) follows its line, dim as every note; one
/// without a note says only that it is kept.
#[test]
fn a_kept_line_says_why_dim() {
    let kept = |note: Option<&str>| Kept {
        line: Line {
            op: Op::Leaf,
            path: "user_data".into(),
            before: Shown::Sensitive(None),
            after: Shown::Sensitive(None),
            leaves: vec![],
            site: None,
            chain: vec![],
            value: None,
            row: None,
        },
        note: note.map(str::to_string),
    };
    let said = |k: &Kept, style: Style| {
        let mut rows = Vec::new();
        super::lines::write_kept(&mut rows, k, "", style, Why::Line);
        rows.remove(0).left
    };
    let replaced = kept(Some(crate::provider::HOLDER_REPLACED));
    assert_eq!(
        said(&replaced, Style::PLAIN),
        "user_data differs (bootstrap): kept  (the key was replaced)"
    );
    assert_eq!(
        said(&replaced, Style { color: true }),
        "user_data differs \x1b[2m(bootstrap): kept\x1b[0m  \x1b[2m(the key was replaced)\x1b[0m"
    );
    assert_eq!(
        said(&kept(None), Style::PLAIN),
        "user_data differs (bootstrap): kept"
    );
}
