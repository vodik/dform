//! A value prints in full when asked (R-176): `why PATH -vv` prints a long
//! value whole (a string's line breaks as lines, a secret still
//! `(sensitive)`), `-v` elides it as the default does, `why --json`
//! carries it whole, and `query PATH` reads a `let` by its path, a used
//! module's too, `--json` whole.

mod common;
use common::Scratch;

/// A rendered cloud-init document of more than 2 KB, in a used module.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "synapse.df",
        "let files = [{ path: \"/etc/f${i}.conf\", content: \"line ${i} of a config file that is long enough\" } \
         | i in 0..30]\n\
         let agent_init = \"#cloud-config\\n${yaml.encode({ package_update: true, write_files: files, \
         runcmd: [\"systemctl enable nftables\"] })}\"\n\
         let token: secret(string) = \"${agent_init}# sealed\"\n",
    );
    s.write(
        "main.df",
        "output init = synapse.agent_init\noutput token: secret(string) = synapse.token\n\nuse synapse\nuse fake\n",
    );
    s
}

#[test]
fn a_long_value_prints_whole_at_vv() {
    let s = project("print-full");
    let json = s
        .run(&["query", "--json", "synapse.agent_init", "main.df"])
        .success();
    let rows: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let doc = rows[0]["value"].as_str().unwrap().to_string();
    assert!(doc.len() > 2048, "{}", doc.len());
    assert!(doc.ends_with("path: /etc/f29.conf\n"), "{doc}");

    // The default and `-v` elide it.
    let short = s.run(&["why", "synapse.agent_init", "main.df"]).success();
    assert!(short.stdout.contains('…'), "{}", short.stdout);
    assert!(!short.stdout.contains("/etc/f17.conf"), "{}", short.stdout);
    let v = s
        .run(&["why", "-v", "synapse.agent_init", "main.df"])
        .success();
    assert_eq!(v.stdout, short.stdout);

    // `-vv`: whole, each line of the document a line, the site after it.
    let vv = s
        .run(&["why", "-vv", "synapse.agent_init", "main.df"])
        .success();
    assert!(!vv.stdout.contains('…'), "{}", vv.stdout);
    let want = format!("let synapse.agent_init = \"{doc}\"");
    let text = vv
        .stdout
        .trim_end()
        .strip_suffix("synapse.df:2")
        .unwrap_or_else(|| panic!("{}", vv.stdout))
        .trim_end();
    assert_eq!(text, want);

    // An output reading it, the same.
    let out = s.run(&["why", "-vv", "init", "main.df"]).success();
    assert!(
        out.stdout.contains("  path: /etc/f29.conf"),
        "{}",
        out.stdout
    );

    // A secret stays `(sensitive)` at `-vv` and in `--json`.
    let token = s.run(&["why", "-vv", "synapse.token", "main.df"]).success();
    assert!(token.stdout.contains("(sensitive"), "{}", token.stdout);
    assert!(!token.stdout.contains("cloud-config"), "{}", token.stdout);

    // `why --json`: the value whole, and what `-vv` prints.
    let j = s
        .run(&["why", "--json", "synapse.agent_init", "main.df"])
        .success();
    let j: serde_json::Value = serde_json::from_str(&j.stdout).unwrap();
    assert_eq!(j[0]["fact"], "let synapse.agent_init");
    assert_eq!(j[0]["value"], doc.as_str());
    assert_eq!(j[0]["text"], vv.stdout.as_str());
    let j = s
        .run(&["why", "--json", "synapse.token", "main.df"])
        .success();
    assert!(!j.stdout.contains("cloud-config"), "{}", j.stdout);
}

/// `query PATH` reads a cell by its path in the result-set form: the
/// program's own `let`, a used module's, an output.
#[test]
fn query_reads_a_cell_by_its_path() {
    let s = project("print-query");
    let r = s.run(&["query", "synapse.agent_init", "main.df"]).success();
    let mut lines = r.stdout.lines();
    assert_eq!(lines.next(), Some("value"), "{}", r.stdout);
    assert!(
        lines
            .next()
            .is_some_and(|l| l.starts_with("\"#cloud-config")),
        "{}",
        r.stdout
    );
    let r = s.run(&["query", "--json", "init", "main.df"]).success();
    assert!(r.stdout.contains("/etc/f29.conf"), "{}", r.stdout);
}
