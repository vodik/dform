//! Where a provider dform.toml names without a `source` is found: its
//! executable `dform-provider-NAME`, or its wasm provider
//! `dform-provider-NAME.wasm`, beside dform's own executable (a release
//! archive's), else in the user's data directory (`cargo xtask
//! install`'s), else in a system data directory, else in the install's
//! lib directory (a package's), else in the checkout's target directory,
//! else on `PATH`, else at its `path`. Only a process proves it: the
//! first place is the running executable's directory.

use crate::common;
use common::{Run, Scratch};
use std::path::Path;

/// A `dform` linked into `s/bin`, to run in `s` with `path` as its
/// `PATH` and data directories of its own: `$XDG_DATA_HOME` `s/data`
/// (`s/home/.local/share` unset), `$XDG_DATA_DIRS` `s/share`.
fn dform(s: &Scratch, path: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(s.path("bin/dform"));
    c.current_dir(&s.dir)
        .env("PATH", path)
        .env("HOME", s.path("home"))
        .env("XDG_DATA_HOME", s.path("data"))
        .env("XDG_DATA_DIRS", s.path("share"));
    c
}

/// `c` run with `args`.
fn run(c: &mut std::process::Command, args: &[&str]) -> Run {
    Run::from(c.args(args).output().unwrap())
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
fn a_provider_is_found_beside_dform_then_in_the_checkout_then_on_path_unless_its_path_is_given() {
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
         lab = {}\npinned = { path = \"explicit/dform-provider-lab\" }\novh = {}\naws = { source = \"aws-mock\" }\n",
    );
    s.write("main.df", "use lab\n\nresource lab.thing a { n = 1 }\n");
    let on_path = s.path("on-path");

    let r = run(&mut dform(&s, &on_path), &["provider", "list"]).success();
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
    // An entry's own `path` is the user's word: it wins over every
    // place the lookup would try.
    let explicit = s.path("explicit/dform-provider-lab");
    assert_eq!(
        row(&r, "pinned"),
        ("its path".into(), explicit.display().to_string())
    );
    // The plan starts the one found.
    let r = run(&mut dform(&s, &on_path), &["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains(&format!("{} exited before its handshake", beside.display())),
        "{}",
        r.stderr
    );

    std::fs::remove_file(&beside).unwrap();
    let r = run(&mut dform(&s, &on_path), &["provider", "list"]).success();
    let found = s.path("on-path/dform-provider-lab");
    assert_eq!(row(&r, "lab"), ("PATH".into(), found.display().to_string()));
}

/// The places between dform's own directory and the checkout's, in
/// order: the user's data directory (its default `~/.local/share` when
/// `$XDG_DATA_HOME` is unset), a system data directory, the install's
/// lib; a wasm provider is found in its place as an executable is, and
/// before the checkout's executable.
#[test]
fn a_provider_is_found_in_the_users_data_directory_then_the_systems_then_the_installs_lib() {
    let s = Scratch::in_target("lookup", "data");
    std::fs::create_dir_all(s.path("bin")).unwrap();
    std::fs::hard_link(env!("CARGO_BIN_EXE_dform"), s.path("bin/dform")).unwrap();
    let dirs = [
        "data/dform/providers",
        "home/.local/share/dform/providers",
        "share/dform/providers",
        "lib/dform/providers",
    ];
    for dir in dirs {
        std::fs::create_dir_all(s.path(dir)).unwrap();
        let exe = s.path(&format!("{dir}/dform-provider-lab"));
        std::os::unix::fs::symlink("/bin/false", exe).unwrap();
    }
    // The checkout's target directory holds the OVH provider's
    // executable; the user's data directory its wasm provider.
    let ovh = s.write("data/dform/providers/dform-provider-ovh.wasm", "\0asm");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nlab = {}\novh = {}\n",
    );
    let nowhere = s.path("nowhere");
    let list = || run(&mut dform(&s, &nowhere), &["provider", "list"]).success();
    let lab = |dir: &str| {
        s.path(&format!("{dir}/dform-provider-lab"))
            .display()
            .to_string()
    };

    let r = list();
    assert_eq!(
        row(&r, "lab"),
        ("XDG_DATA_HOME".into(), lab("data/dform/providers"))
    );
    assert_eq!(
        row(&r, "ovh"),
        ("XDG_DATA_HOME".into(), ovh.display().to_string())
    );
    let mut unset = dform(&s, &nowhere);
    let r = run(unset.env_remove("XDG_DATA_HOME"), &["provider", "list"]).success();
    assert_eq!(
        row(&r, "lab"),
        (
            "XDG_DATA_HOME".into(),
            lab("home/.local/share/dform/providers")
        )
    );
    std::fs::remove_file(lab("data/dform/providers")).unwrap();
    assert_eq!(
        row(&list(), "lab"),
        ("XDG_DATA_DIRS".into(), lab("share/dform/providers"))
    );
    std::fs::remove_file(lab("share/dform/providers")).unwrap();
    assert_eq!(
        row(&list(), "lab"),
        (
            "the install's lib directory".into(),
            lab("lib/dform/providers")
        )
    );
    // Outside a project, every provider found, each where it is found.
    std::fs::remove_file(s.path("dform.toml")).unwrap();
    let r = list();
    assert_eq!(
        row(&r, "ovh"),
        ("XDG_DATA_HOME".into(), ovh.display().to_string())
    );
    assert_eq!(
        row(&r, "lab"),
        (
            "the install's lib directory".into(),
            lab("lib/dform/providers")
        )
    );
}
