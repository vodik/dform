//! Paths (R-65): a module is named by its path from the project root,
//! looked up, never searched, and loaded once however often it is named;
//! `[packages.NAME]` mounts another project under a name.

mod common;
use common::Scratch;

/// The same module named from two files is one file: were it loaded
/// twice, its items would be declared twice.
#[test]
fn a_module_named_twice_loads_once() {
    let s = Scratch::new("lang-paths");
    s.write("lib/types.df", "\ntype tier = enum(\"gold\")\n");
    s.write("lib/more.df", "\nuse lib.types\nlet best = \"gold\"\n");
    s.write(
        "p.df",
        "\ninput t: types.tier = \"gold\"\nuse lib.types\nuse lib.more\nuse fake\n",
    );
    let files = dform::loader::program_files(&[s.path("p.df")]).unwrap();
    let types = files.iter().filter(|f| f.ends_with("lib/types.df")).count();
    assert_eq!(types, 1, "{files:?}");
    s.run(&["dev", "--world", "w.json", "plan", "p.df"])
        .success();
}

/// `use lan` is `lan.df` beside the root, never `modules/lan.df`: the
/// error names the files a path could have been.
#[test]
fn a_path_is_looked_up_never_searched() {
    let s = Scratch::project("lang-paths-lookup");
    s.write("modules/lan.df", "\nlet cidr = \"10.0.0.0/16\"\n");
    s.write("stacks/app.df", "\nuse lan\nuse fake\n");
    let r = s.run(&["plan", "app"]).failure();
    assert!(
        r.stderr.contains("no module `lan`: there is no lan.df"),
        "{}",
        r.stderr
    );
    s.write("stacks/app.df", "\nuse modules.lan\nuse fake\n");
    s.run(&["plan", "app"]).success();
}

/// `[packages.infra] path = "../infra"` mounts another project at
/// `infra`: its modules are `infra.config`.
#[test]
fn a_package_mounts_another_project() {
    let s = Scratch::new("lang-paths-packages");
    s.write(
        "infra/dform.toml",
        "[project]\nedition = \"2026\"\nname = \"infra\"\n",
    );
    s.write("infra/config.df", "\nlet domain = \"example.org\"\n");
    s.write(
        "app/dform.toml",
        "[project]\nedition = \"2026\"\nname = \"app\"\n\n[packages.infra]\npath = \"../infra\"\n",
    );
    s.write(
        "app/stacks/web.df",
        "\nuse fake\nuse infra.config\nresource compute.vm web {\n  tags = { domain: config.domain }\n}\n",
    );
    let r = s.run_in("app", &["plan", "web"]).success();
    assert!(
        r.stdout.contains("tags.domain = \"example.org\""),
        "{}",
        r.stdout
    );
}
