//! Where a provider dform.toml names without a `source` is found: its
//! executable `dform-provider-NAME` beside dform's own executable (a
//! release archive's), else in the checkout's target directory, else on
//! `PATH`, else at its `path`. Only a process proves it: the first place
//! is the running executable's directory.

mod common;
use common::{Run, Scratch};
use std::path::Path;

/// A `dform` linked into `s/bin`, run in `s` with `path` as its `PATH`.
fn dform(s: &Scratch, path: &Path, args: &[&str]) -> Run {
    let mut c = std::process::Command::new(s.path("bin/dform"));
    c.args(args).current_dir(&s.dir).env("PATH", path);
    Run::from(c.output().unwrap())
}

/// `provider list`'s row of `name`: where it was found and the path.
fn row(r: &Run, name: &str) -> (String, String) {
    let line = r
        .stdout
        .lines()
        .find(|l| l.split_whitespace().next() == Some(name))
        .unwrap_or_else(|| panic!("no row of {name}:\n{}", r.stdout));
    let rest = line[name.len()..].trim();
    let (from, path) = rest.rsplit_once("  ").unwrap();
    (from.trim().to_string(), path.trim().to_string())
}

#[test]
fn a_provider_is_found_beside_dform_then_in_the_checkout_then_on_path_then_at_its_path() {
    let s = Scratch::in_target("lookup", "order");
    std::fs::create_dir_all(s.path("bin")).unwrap();
    std::fs::create_dir_all(s.path("on-path")).unwrap();
    std::fs::create_dir_all(s.path("explicit")).unwrap();
    // Linked, not copied: the same filesystem as the build, and nothing
    // written just before its exec (ETXTBSY). Each provider is `false`,
    // which exits before its handshake, naming the one dform started.
    std::fs::hard_link(env!("CARGO_BIN_EXE_dform"), s.path("bin/dform")).unwrap();
    for dir in ["bin", "on-path", "explicit"] {
        let exe = s.path(&format!("{dir}/dform-provider-lab"));
        std::os::unix::fs::symlink("/bin/false", exe).unwrap();
    }
    // The checkout's own: the build's target directory holds the OVH
    // provider, and nothing beside the linked dform does.
    let ovh = common::exe("dform-provider-ovh");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\n\
         lab = { path = \"explicit/dform-provider-lab\" }\novh = {}\naws = { source = \"aws-mock\" }\n",
    );
    s.write("main.df", "use lab\n\nresource lab.thing a { n = 1 }\n");
    let on_path = s.path("on-path");

    let r = dform(&s, &on_path, &["provider", "list"]).success();
    let beside = s.path("bin/dform-provider-lab");
    assert_eq!(
        row(&r, "lab"),
        ("beside dform".into(), beside.display().to_string())
    );
    assert_eq!(
        row(&r, "ovh"),
        ("the checkout's target directory".into(), ovh.clone())
    );
    assert_eq!(
        row(&r, "aws"),
        ("a schema the mock plays".into(), "aws-mock".into())
    );
    // The plan starts the one found.
    let r = dform(&s, &on_path, &["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains(&format!("{} exited before its handshake", beside.display())),
        "{}",
        r.stderr
    );

    std::fs::remove_file(&beside).unwrap();
    let r = dform(&s, &on_path, &["provider", "list"]).success();
    let found = s.path("on-path/dform-provider-lab");
    assert_eq!(row(&r, "lab"), ("PATH".into(), found.display().to_string()));

    let r = dform(&s, Path::new(""), &["provider", "list"]).success();
    let explicit = s.path("explicit/dform-provider-lab");
    assert_eq!(
        row(&r, "lab"),
        ("its path".into(), explicit.display().to_string())
    );
}
