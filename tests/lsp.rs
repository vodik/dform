//! `dform lsp` over stdio, on copies of examples/demo and examples/pngu:
//! the contributors hover and `dform.why`, diagnostics of the selected
//! environment (`dform.selectEnvironment`), schema completion, quick
//! fixes, references and rename (a rename with state plans as a move),
//! and the free ones (parse diagnostics, formatting, go-to-definition).
//! The server evaluates read only: the copies gain no dform.state/ but by
//! an explicit apply.

mod common;

use common::{Scratch, copy_dir, repo};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// A client speaking JSON-RPC to a `dform lsp` process.
struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
    /// Notifications received, in order.
    notes: Vec<Value>,
}

impl Client {
    fn start(root: &Path, options: Value) -> Client {
        Client::start_with(root, options, common::dform())
    }

    /// A server run as `command` (its environment) says.
    fn start_with(root: &Path, options: Value, mut command: Command) -> Client {
        let mut child = command
            .arg("lsp")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut c = Client {
            child,
            stdin,
            stdout,
            next: 0,
            notes: Vec::new(),
        };
        let init = c.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": uri(root),
                "capabilities": {},
                "initializationOptions": options,
            }),
        );
        let commands = &init["capabilities"]["executeCommandProvider"]["commands"];
        assert_eq!(
            commands,
            &json!(["dform.selectEnvironment", "dform.why"]),
            "{init}"
        );
        c.notify("initialized", json!({}));
        c
    }

    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut len = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "server exited"
            );
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length: ") {
                len = n.parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        self.stdout.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// The result of a request; notifications meanwhile are kept.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let m = self.recv();
            if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
                assert!(m.get("error").is_none(), "{method}: {m}");
                return m["result"].clone();
            }
            self.notes.push(m);
        }
    }

    /// The next notification `method` whose params satisfy `pred`, waiting
    /// for it.
    fn wait(&mut self, method: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self
            .notes
            .iter()
            .position(|n| n["method"] == method && pred(&n["params"]))
        {
            return self.notes.remove(i)["params"].clone();
        }
        loop {
            let m = self.recv();
            if m["method"] == method && pred(&m["params"]) {
                return m["params"].clone();
            }
            self.notes.push(m);
        }
    }

    /// The diagnostics last published for `file`, after the next
    /// publication for it.
    fn diagnostics(&mut self, file: &Path) -> Vec<Value> {
        let u = uri(file);
        let p = self.wait("textDocument/publishDiagnostics", |p| p["uri"] == u);
        p["diagnostics"].as_array().unwrap().clone()
    }

    fn open(&mut self, file: &Path) {
        let text = std::fs::read_to_string(file).unwrap();
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri(file), "languageId": "dform", "version": 1, "text": text
            }}),
        );
    }

    fn change(&mut self, file: &Path, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri(file), "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    fn at(&mut self, method: &str, file: &Path, (line, character): (u32, u32)) -> Value {
        self.request(
            method,
            json!({
                "textDocument": { "uri": uri(file) },
                "position": { "line": line, "character": character },
            }),
        )
    }

    fn command(&mut self, command: &str, arguments: Value) -> Value {
        self.request(
            "workspace/executeCommand",
            json!({ "command": command, "arguments": arguments }),
        )
    }

    fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}

fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

/// A copy of the example project `name`, its path canonical (as the server
/// names files).
fn example(name: &str) -> (Scratch, PathBuf) {
    let s = Scratch::new(&format!("lsp-{name}"));
    let root = s.dir.join(name);
    copy_dir(&repo().join("examples").join(name), &root);
    let root = std::fs::canonicalize(&root).unwrap();
    (s, root)
}

/// The (line, character) of the first `needle` in `file`, plus `ahead`
/// characters.
fn find(file: &Path, needle: &str, ahead: u32) -> (u32, u32) {
    let text = std::fs::read_to_string(file).unwrap();
    let at = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {}", file.display()));
    let p = dform_lsp::text::position(&text, at);
    (p.line, p.character + ahead)
}

fn labels(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap().to_string())
        .collect()
}

fn messages(ds: &[Value]) -> Vec<String> {
    ds.iter()
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect()
}

/// The contributors hover and `dform.why` on an attribute a module and a
/// policy pack both contribute to.
#[test]
fn hover_shows_every_contribution_and_why_prints_the_derivation() {
    let (_s, root) = example("demo");
    let network = root.join("network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&network);
    let at = find(&network, "tags = { env", 1);
    let hover = c.at("textDocument/hover", &network, at);
    let text = hover["contents"]["value"].as_str().unwrap().to_string();
    // The deployment is the `dform/environment` notification's, not the
    // hover's.
    assert!(!text.contains("dform[env=staging]"), "{text}");
    // The schema's description of the path.
    assert!(text.contains("Key-value labels on the network."), "{text}");
    assert!(text.contains("**net.vpc[\"main/vpc\"].tags**"), "{text}");
    assert!(text.contains("**net.vpc[\"peer/vpc\"].tags**"), "{text}");
    assert!(text.contains("winning rank: normal"), "{text}");
    // Every contribution: the module's, and the policy pack's.
    assert!(
        text.contains("network.df:19:42, instance network.vpc main"),
        "{text}"
    );
    assert!(text.contains("baseline.df:10:1, use baseline"), "{text}");
    // The derivation in the source form, as `dform why` prints it.
    assert!(
        text.contains(
            "  merged from 2 contributions\n  ├─ {component: \"network\", env: \"staging\"}\n"
        ),
        "{text}"
    );
    assert!(!text.contains("Σattr"), "{text}");

    let why = c.command(
        "dform.why",
        json!([{ "textDocument": { "uri": uri(&network) }, "position": { "line": at.0, "character": at.1 } }]),
    );
    let why = why.as_str().unwrap();
    assert!(
        why.starts_with(
            "net.vpc[\"main/vpc\"].tags = {component: \"network\", env: \"staging\", team: \"platform\"}\n  merged from 2 contributions\n"
        ),
        "{why}"
    );
    // Shown to the user too (eglot discards a command's result).
    let shown = c.wait("window/showMessage", |_| true);
    assert_eq!(shown["message"].as_str(), Some(why));

    // An attr read in a rule's body (`a.cidr`, of a deny that does not
    // fire): the attributes it matches.
    let stdlib = root.join("stdlib_net.df");
    c.open(&stdlib);
    let hover = c.at("textDocument/hover", &stdlib, find(&stdlib, "a.cidr", 3));
    let text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        text.contains("**net.vpc[\"main/vpc\"].cidr** = `10.50.0.0/16`"),
        "{hover}"
    );
    assert!(
        text.contains("network.df:19:26, instance network.vpc peer"),
        "{hover}"
    );
    c.shutdown();
}

/// The hover's content by what is under point: a declared name's doc
/// comment (an input's, an alias's definition, a module instance's inputs
/// and outputs with theirs, an output read through its instance), a
/// builtin's or keyword's reference entry, a schema path's description;
/// and nothing on whitespace, a comment or a string literal.
#[test]
fn hover_shows_docs_builtins_keywords_and_nothing_elsewhere() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let network = root.join("network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.open(&network);
    let hover = |c: &mut Client, file: &Path, needle: &str, ahead: u32| -> Value {
        let h = c.at("textDocument/hover", file, find(file, needle, ahead));
        h["contents"]["value"].clone()
    };

    // Nothing: whitespace, a comment, a string literal, even inside a
    // block or a fact that derives something.
    assert_eq!(hover(&mut c, &network, "{ cidr = vpc_net", 1), Value::Null);
    assert_eq!(hover(&mut c, &stack, "# Both ends", 4), Value::Null);
    assert_eq!(hover(&mut c, &stack, "\"us-test-1a\"", 3), Value::Null);

    // An input read by name: its declaration and doc comment.
    let text = hover(&mut c, &stack, "where env ==", 7);
    let text = text.as_str().unwrap();
    assert!(
        text.contains("key env: environment = \"staging\""),
        "{text}"
    );
    assert!(text.contains("The deployment's environment"), "{text}");
    assert!(text.contains("- **owner**: platform"), "{text}");

    // An alias: its definition.
    let text = hover(&mut c, &stack, "key env: environment", 10);
    let text = text.as_str().unwrap();
    assert!(
        text.contains("type environment = enum(\"dev\", \"staging\", \"prod\")"),
        "{text}"
    );
    assert!(text.contains("The environments: an alias"), "{text}");

    // A component in an instance's path: its docs, its inputs and outputs.
    let text = hover(&mut c, &stack, "instance network.vpc main", 18);
    let text = text.as_str().unwrap();
    assert!(
        text.contains("One VPC, and a private subnet in every zone."),
        "{text}"
    );
    assert!(
        text.contains("- `input vpc_net: inet`: The VPC's IPv4 range"),
        "{text}"
    );
    assert!(
        text.contains("- `output vpc: net.vpc = vpc`: The network's VPC, for peering."),
        "{text}"
    );
    assert!(
        text.contains("- `output private_subnet`: The network's private subnets, one row each"),
        "{text}"
    );

    // An output read through its instance.
    let text = hover(&mut c, &stack, "main.private_subnet", 7);
    assert!(
        text.as_str().unwrap().contains("output private_subnet"),
        "{text}"
    );

    // A builtin, a keyword.
    let text = hover(&mut c, &stack, "inet.host(", 2);
    let text = text.as_str().unwrap();
    assert!(
        text.contains("inet.host(net: inet, n: int) -> ip"),
        "{text}"
    );
    assert!(text.contains("usable host"), "{text}");
    let text = hover(&mut c, &stack, "} where env != \"dev\"", 4);
    assert!(
        text.as_str()
            .unwrap()
            .contains("HEAD where BODY | BLOCK where BODY"),
        "{text}"
    );

    // A relation: its columns, declared or inferred (R-34).
    let text = hover(&mut c, &stack, "vpc_peer_inst(\"main\"", 2);
    assert!(
        text.as_str()
            .unwrap()
            .contains("```dform\nvpc_peer_inst(string, string)\n```"),
        "{text}"
    );
    let text = hover(&mut c, &network, "zone_index(\"us-test-1a\"", 2);
    assert!(
        text.as_str().unwrap().contains("zone_index(string, int)"),
        "{text}"
    );

    // A schema type: its description.
    let text = hover(&mut c, &network, "resource net.vpc vpc", 11);
    assert!(
        text.as_str().unwrap().contains("A virtual network"),
        "{text}"
    );
    c.shutdown();
}

/// A refinement's `check` is a word only where it opens one: its hover is
/// the reference entry; a `where` clause's word is the keyword's.
#[test]
fn hover_on_the_clause_and_refinement_words() {
    let (_s, root) = example("refine");
    let stack = root.join("stacks/refine_gke.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    let hover = |c: &mut Client, needle: &str, ahead: u32| -> String {
        let h = c.at("textDocument/hover", &stack, find(&stack, needle, ahead));
        h["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let text = hover(&mut c, "check zones >= 1", 2);
    assert!(text.contains("A refinement"), "{text}");
    let text = hover(&mut c, "} where ", 4);
    assert!(text.contains("HEAD where BODY"), "{text}");
    c.shutdown();
}

/// Signature help on a builtin's call and on an extern's (its declaration's
/// parameters and doc comment), with the argument point is in; a call being
/// typed has one too.
#[test]
fn signature_help_of_builtins_and_externs() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    let help = c.at(
        "textDocument/signatureHelp",
        &stack,
        find(&stack, "cidrs.main), 20)", 12),
    );
    assert_eq!(
        help["signatures"][0]["label"], "inet.host(net: inet, n: int) -> ip?",
        "{help}"
    );
    assert_eq!(help["activeParameter"], 1, "{help}");

    let text = std::fs::read_to_string(&stack).unwrap();
    let typed = format!(
        "{text}\n#| The address a name resolves to.\nextern dns.lookup(+name: string, -addr: string)\nx(a) where dns.lookup(\"db\", "
    );
    c.change(&stack, 2, &typed);
    let n = typed.lines().count() as u32;
    let last = typed.lines().last().unwrap().len() as u32;
    let help = c.at("textDocument/signatureHelp", &stack, (n - 1, last));
    let sig = &help["signatures"][0];
    assert_eq!(
        sig["label"], "dns.lookup(+name: string, -addr: string)",
        "{help}"
    );
    assert_eq!(
        sig["documentation"]["value"],
        "The address a name resolves to."
    );
    assert_eq!(help["activeParameter"], 1, "{help}");

    // Outside every call: none.
    let help = c.at(
        "textDocument/signatureHelp",
        &stack,
        find(&stack, "key env", 3),
    );
    assert_eq!(help, Value::Null);
    c.shutdown();
}

/// Diagnostics are of the selected environment: a deny that fires only in
/// prod appears once prod is selected, at the rule that fired; a conflict
/// is published at a contribution, the contributors as related
/// information; a syntax error is published as the edit is made.
#[test]
fn diagnostics_follow_the_selected_environment() {
    let (s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    let ds = c.diagnostics(&stack);
    assert!(
        ds.iter().all(|d| d["severity"] != 1),
        "the demo plans clean: {ds:?}"
    );

    // The environments a picker offers.
    let choices = c.command("dform.selectEnvironment", json!([]));
    let offered = labels(&choices["choices"]);
    for want in ["default", "env=dev", "env=staging", "env=prod"] {
        assert!(offered.contains(&want.to_string()), "{want} in {offered:?}");
    }

    let original = std::fs::read_to_string(&stack).unwrap();
    let edited = format!("{original}\ndeny \"prod is frozen\" where env == \"prod\"\n");
    c.change(&stack, 2, &edited);
    let ds = c.diagnostics(&stack);
    assert!(
        !messages(&ds).contains(&"prod is frozen".to_string()),
        "{ds:?}"
    );

    let status = c.command("dform.selectEnvironment", json!(["env=prod"]));
    assert_eq!(status["label"], "env=prod");
    assert_eq!(status["deployments"], json!(["dform[env=prod]"]));
    let note = c.wait("dform/environment", |_| true);
    assert_eq!(note["label"], "env=prod");
    let ds = c.diagnostics(&stack);
    let frozen = ds
        .iter()
        .find(|d| d["message"] == "prod is frozen")
        .unwrap_or_else(|| panic!("{ds:?}"));
    let deny_line = edited
        .lines()
        .position(|l| l.starts_with("deny \"prod is frozen"))
        .unwrap();
    assert_eq!(frozen["range"]["start"]["line"], deny_line, "{frozen}");
    assert_eq!(frozen["severity"], 1);

    // A map of key values selects too; back to staging, the deny is gone.
    c.command("dform.selectEnvironment", json!([{ "env": "staging" }]));
    let ds = c.diagnostics(&stack);
    assert!(
        !messages(&ds).contains(&"prod is frozen".to_string()),
        "{ds:?}"
    );

    // A conflict: two contributions to bastion's private_ip.
    let conflicted =
        format!("{original}\nresource compute.vm bastion {{\n  private_ip = \"10.9.9.9\"\n}}\n");
    c.change(&stack, 3, &conflicted);
    let ds = c.diagnostics(&stack);
    let conflict = ds
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("conflicting attribute contributions"))
        })
        .unwrap_or_else(|| panic!("{ds:?}"));
    let related = conflict["relatedInformation"].as_array().unwrap();
    let lines: Vec<u64> = related
        .iter()
        .map(|r| r["location"]["range"]["start"]["line"].as_u64().unwrap())
        .collect();
    let added = conflicted
        .lines()
        .position(|l| l.contains("10.9.9.9"))
        .unwrap() as u64;
    let own = original
        .lines()
        .position(|l| l.contains("private_ip = inet.host"))
        .unwrap() as u64;
    assert!(lines.contains(&added) && lines.contains(&own), "{conflict}");

    // A syntax error, as typed.
    c.change(&stack, 4, &format!("{original}\nresource compute.vm {{\n"));
    let ds = c.diagnostics(&stack);
    assert!(ds.iter().any(|d| d["severity"] == 1), "{ds:?}");
    c.shutdown();
    assert!(!root.join("dform.state").exists(), "the server wrote state");
    drop(s);
}

/// Schema completion: a resource block's paths with their type, flags and
/// refinements; resource types; an instance's component inputs, and outputs
/// after `copy.`.
#[test]
fn completion_reads_the_schema_and_the_modules() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let database = root.join("database.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.open(&database);

    let network = root.join("network.df");
    c.open(&network);
    let items = c.at(
        "textDocument/completion",
        &network,
        find(&network, "cidr = vpc_net", 0),
    );
    let names = labels(&items);
    assert!(names.contains(&"cidr".to_string()), "{names:?}");
    // The schema's description of each path.
    let cidr = items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "cidr")
        .unwrap();
    assert!(
        cidr["documentation"]
            .as_str()
            .unwrap()
            .starts_with("The network's IPv4 range"),
        "{cidr}"
    );
    assert!(
        !names.contains(&"id".to_string()),
        "computed paths are not written: {names:?}"
    );

    // A refinement from the schema facts: db.postgres's backup_days.
    let db = std::fs::read_to_string(&database).unwrap();
    let block = db.find("resource db.postgres").unwrap();
    let line = db[..block].matches('\n').count() as u32 + 1;
    let items = c.at("textDocument/completion", &database, (line, 4));
    let backup = items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "backup_days")
        .unwrap_or_else(|| panic!("{items}"));
    assert!(
        backup["documentation"]
            .as_str()
            .unwrap()
            .contains("range(1, 35)"),
        "{backup}"
    );

    // An instance block: its component's inputs.
    let at = find(&stack, "instance network.vpc main { ", 28);
    let items = c.at("textDocument/completion", &stack, at);
    assert!(labels(&items).contains(&"vpc_net".to_string()), "{items}");

    // `resource `, `main.`, as typed.
    let original = std::fs::read_to_string(&stack).unwrap();
    let typed = format!("{original}\nresource \nx = main.\n");
    c.change(&stack, 2, &typed);
    let n = typed.lines().count() as u32;
    let types = labels(&c.at("textDocument/completion", &stack, (n - 2, 9)));
    assert!(types.contains(&"net.vpc".to_string()), "{types:?}");
    let outputs = labels(&c.at("textDocument/completion", &stack, (n - 1, 9)));
    // Its exported relation too (R-55).
    assert_eq!(outputs, vec!["vpc", "private_subnet"], "{outputs:?}");

    // A plain word: the builtins it starts, their signatures.
    let typed = format!("{original}\ny = iprang");
    c.change(&stack, 3, &typed);
    let n = typed.lines().count() as u32;
    let items = c.at("textDocument/completion", &stack, (n - 1, 10));
    assert_eq!(labels(&items), vec!["iprange"], "{items}");
    assert_eq!(items[0]["detail"], "iprange(a: ip, b: ip) -> iprange");

    c.shutdown();
}

/// Go-to-definition of a component, a module by its path (R-65) and a
/// predicate; formatting by `dform fmt`'s formatter.
#[test]
fn definition_and_formatting() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);

    let def = |c: &mut Client, needle: &str, ahead: u32| -> Vec<(String, u64)> {
        let at = find(&stack, needle, ahead);
        let locs = c.at("textDocument/definition", &stack, at);
        locs.as_array()
            .unwrap()
            .iter()
            .map(|l| {
                let u = l["uri"].as_str().unwrap();
                let file = u.rsplit('/').take(2).collect::<Vec<_>>();
                (
                    format!("{}/{}", file[1], file[0]),
                    l["range"]["start"]["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let component = std::fs::read_to_string(root.join("network.df"))
        .unwrap()
        .lines()
        .position(|l| l.starts_with("component vpc"))
        .unwrap() as u64;
    assert_eq!(
        def(&mut c, "instance network.vpc main", 18),
        vec![("demo/network.df".into(), component)]
    );
    assert_eq!(
        def(&mut c, "use baseline", 7),
        vec![("demo/baseline.df".into(), 0)]
    );
    let text = std::fs::read_to_string(&stack).unwrap();
    let head = text
        .lines()
        .position(|l| l.starts_with("vpc_peer_pair(ia"))
        .unwrap() as u64;
    assert_eq!(
        def(&mut c, "vpc_peer_pair(_, _", 2),
        vec![("stacks/dform.df".into(), head)]
    );
    assert!(
        def(&mut c, "network.vpc[ia].vpc", 1).contains(&("demo/network.df".into(), 0)),
        "a module read by the path of its component"
    );

    let messy = "q(1)\np(x)   where q(x)\n";
    let scratch = root.join("stacks/messy.df");
    std::fs::write(&scratch, messy).unwrap();
    c.open(&scratch);
    let edits = c.request(
        "textDocument/formatting",
        json!({ "textDocument": { "uri": uri(&scratch) }, "options": { "tabSize": 2, "insertSpaces": true } }),
    );
    assert_eq!(edits[0]["newText"], "q(1)\np(x) where q(x)\n", "{edits}");
    c.shutdown();
}

/// examples/pngu: keyed by env, one deployment per value; selecting
/// env=prod evaluates pngu[env=prod]. And the latency of an edit: each
/// keystroke's re-evaluation, timed by the server (`dform/stats`) and as
/// the client waits for the hover after it.
#[test]
fn pngu_by_environment_and_latency_per_keystroke() {
    let (_s, root) = example("pngu");
    let stack = root.join("stacks/pngu.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.diagnostics(&stack);
    let status = c.command("dform.selectEnvironment", json!(["env=prod"]));
    assert_eq!(status["deployments"], json!(["pngu[env=prod]"]), "{status}");
    let errors: Vec<Value> = c
        .diagnostics(&stack)
        .into_iter()
        .filter(|d| d["severity"] == 1)
        .collect();
    assert!(errors.is_empty(), "pngu env=prod plans: {errors:?}");
    c.shutdown();

    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let network = root.join("network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.open(&network);
    c.diagnostics(&stack);
    let at = find(&network, "tags = { env", 1);
    let text = std::fs::read_to_string(&stack).unwrap();
    let mut typed = text.clone();
    typed.push_str("\n# ");
    let mut waits = Vec::new();
    for (i, ch) in "keystroke".chars().enumerate() {
        typed.push(ch);
        c.change(&stack, i as i32 + 2, &typed);
        let start = Instant::now();
        let hover = c.at("textDocument/hover", &network, at);
        waits.push(start.elapsed());
        assert!(hover.is_object());
    }
    let stats = c.request("dform/stats", Value::Null);
    let ms: Vec<f64> = stats["ms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let per_key = &ms[ms.len() - waits.len()..];
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    eprintln!(
        "lsp latency per keystroke on examples/demo ({} build): evaluation {:?} ms; hover round trip {:?} ms; loadavg {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        per_key.iter().map(|m| m.round() as u64).collect::<Vec<_>>(),
        waits.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        load.trim()
    );
    assert!(waits.iter().all(|d| *d < Duration::from_secs(30)));
    c.shutdown();
}

/// Byte offset of an LSP position (UTF-16 columns) in `text`.
fn offset_of(text: &str, pos: &Value) -> usize {
    dform_lsp::text::offset(text, serde_json::from_value(pos.clone()).unwrap())
}

/// A quick fix end to end: with `edited` as `file`'s text, a diagnostic
/// whose message contains `needle` is published; the code action titled
/// `title...` is offered for it; applied (to the files), the diagnostic
/// is gone and `dform fmt --check` passes on every file it edited. The
/// edited files' texts.
fn quick_fix(root: &Path, file: &Path, edited: &str, needle: &str, title: &str) -> Vec<String> {
    std::fs::write(file, edited).unwrap();
    let mut c = Client::start(root, json!({}));
    c.open(file);
    let ds = c.diagnostics(file);
    let d = ds
        .iter()
        .find(|d| d["message"].as_str().unwrap().contains(needle))
        .unwrap_or_else(|| panic!("{needle}: {ds:?}"))
        .clone();
    let actions = c.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": uri(file) },
            "range": d["range"],
            "context": { "diagnostics": [d] },
        }),
    );
    let action = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"].as_str().unwrap().starts_with(title))
        .unwrap_or_else(|| panic!("{title}: {actions}"))
        .clone();
    assert_eq!(action["kind"], "quickfix", "{action}");
    assert_eq!(action["diagnostics"], json!([d]), "{action}");
    let mut texts = Vec::new();
    for (u, edits) in action["edit"]["changes"].as_object().unwrap() {
        let path = PathBuf::from(u.strip_prefix("file://").unwrap());
        let mut text = std::fs::read_to_string(&path).unwrap();
        for e in edits.as_array().unwrap().iter().rev() {
            let start = offset_of(&text, &e["range"]["start"]);
            let end = offset_of(&text, &e["range"]["end"]);
            text.replace_range(start..end, e["newText"].as_str().unwrap());
        }
        std::fs::write(&path, &text).unwrap();
        if path == file {
            c.change(file, 2, &text);
        }
        // A program stays formatted; dform.toml is not a program.
        if path.extension().is_some_and(|e| e == "df") {
            let fmt = common::dform()
                .args(["fmt", "--check"])
                .arg(&path)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                fmt.status.success(),
                "fmt --check {}: {}\n{text}",
                path.display(),
                String::from_utf8_lossy(&fmt.stderr)
            );
        }
        texts.push(text);
    }
    c.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri(file) } }),
    );
    let ds = c.diagnostics(file);
    assert!(
        !messages(&ds).iter().any(|m| m.contains(needle)),
        "{needle} after {title}: {ds:?}"
    );
    c.shutdown();
    texts
}

/// An unknown name: quoted.
#[test]
fn quick_fix_quotes_an_unknown_name() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let text = std::fs::read_to_string(&stack).unwrap();
    let edited =
        format!("{text}\nresource net.vpc extra {{ cidr = \"10.1.0.0/16\", name = bogus }}\n");
    let texts = quick_fix(
        &root,
        &stack,
        &edited,
        "unknown name `bogus`",
        "quote it: \"bogus\"",
    );
    assert!(texts[0].contains(" name = \"bogus\" }\n"), "{}", texts[0]);
}

/// A predicate with both facts and rules: `decl p(..) mixed` before them,
/// then placed in the header with the file's other `decl`s (R-11a) when
/// the edit is formatted (the file already was).
#[test]
fn quick_fix_declares_a_predicate_mixed() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let text = std::fs::read_to_string(&stack).unwrap();
    let edited = format!("{text}\nq(1)\nq(x) where data(\"zone\", x)\n");
    let texts = quick_fix(
        &root,
        &stack,
        &edited,
        "q/1 has both ground facts and rules",
        "declare it: `decl q(a) mixed`",
    );
    assert!(
        texts[0].contains(
            "input cidrs { main: string = \"10.50.0.0/16\", peer: string = \"10.60.0.0/16\" }\n\
             decl q(a) mixed\n"
        ),
        "{}",
        texts[0]
    );
    assert!(
        texts[0].ends_with("\nq(1)\nq(x) where data(\"zone\", x)\n"),
        "{}",
        texts[0]
    );
}

/// The collision lint (a warning in the demo): the key interpolated
/// into the name, or the stack said isolated.
#[test]
fn quick_fix_derives_a_colliding_name_from_the_key_or_isolates_the_stack() {
    let collides = |root: &Path| {
        let manifest = root.join("dform.toml");
        let toml = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(&manifest, toml.replace("isolated = true\n", "")).unwrap();
        let stack = root.join("stacks/dform.df");
        let text = std::fs::read_to_string(&stack).unwrap();
        let edited = format!(
            "{text}\nresource net.vpc fixed {{ cidr = \"10.1.0.0/16\", name = \"fixed\" }}\n"
        );
        (stack, edited)
    };
    let needle = "net.vpc[\"fixed\"].name = \"fixed\" does not depend on the stack's key (env)";
    let (_s, root) = example("demo");
    let (stack, edited) = collides(&root);
    let texts = quick_fix(
        &root,
        &stack,
        &edited,
        needle,
        "derive the name from the key",
    );
    assert!(
        texts[0].contains(" name = \"fixed-${env}\" }\n"),
        "{}",
        texts[0]
    );

    let (_s, root) = example("demo");
    let (stack, edited) = collides(&root);
    let texts = quick_fix(
        &root,
        &stack,
        &edited,
        needle,
        "say `isolated = true` in dform.toml",
    );
    assert!(
        texts[0].contains("[stacks.dform]\nisolated = true\n"),
        "{}",
        texts[0]
    );
}

/// A required attribute no contribution sets (the provider refuses the
/// plan, at the top of the stack's file): set, with a typed placeholder.
#[test]
fn quick_fix_sets_a_required_attribute() {
    let (_s, root) = example("k8s");
    let stack = root.join("stacks/k8s_demo.df");
    let text = std::fs::read_to_string(&stack).unwrap();
    let edited = format!(
        "{text}\nresource k8s.persistent_volume_claim data {{ metadata.name = \"data\" }}\n"
    );
    let texts = quick_fix(
        &root,
        &stack,
        &edited,
        "k8s.persistent_volume_claim[\"data\"]: required attribute spec.accessModes is not set",
        "set the required spec.accessModes, spec.resources.requests.storage",
    );
    assert!(
        texts[0].ends_with(
            "  metadata.name = \"data\"\n  spec.accessModes = []\n  spec.resources.requests.storage = 0\n}\n"
        ),
        "{}",
        texts[0]
    );
}

/// A ref to an address no rule wants: the block guarded on it.
#[test]
fn quick_fix_guards_a_dangling_ref() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let text = std::fs::read_to_string(&stack).unwrap();
    let edited = format!(
        "{text}\nresource net.subnet extra {{\n  cidr = \"10.0.1.0/24\"\n  vpc_id = ref(net.vpc, \"other\", \"id\")\n}}\n"
    );
    let texts = quick_fix(
        &root,
        &stack,
        &edited,
        "ref to an address no rule wants",
        "guard the block on net.vpc other existing",
    );
    assert!(
        texts[0]
            .contains("vpc_id = ref(net.vpc, \"other\", \"id\")\n} where \"other\" in net.vpc\n"),
        "{}",
        texts[0]
    );
}

/// Locations as `(file relative to root, 1-based line)`, sorted.
fn places(root: &Path, locs: &Value) -> Vec<(String, u64)> {
    let prefix = uri(root) + "/";
    let mut out: Vec<(String, u64)> = locs
        .as_array()
        .unwrap_or_else(|| panic!("{locs}"))
        .iter()
        .map(|l| {
            let u = l["uri"].as_str().unwrap();
            (
                u.strip_prefix(&prefix).unwrap_or(u).to_string(),
                l["range"]["start"]["line"].as_u64().unwrap() + 1,
            )
        })
        .collect();
    out.sort();
    out
}

fn references(c: &mut Client, root: &Path, file: &Path, at: (u32, u32)) -> Vec<(String, u64)> {
    let locs = c.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri(file) },
            "position": { "line": at.0, "character": at.1 },
            "context": { "includeDeclaration": true },
        }),
    );
    places(root, &locs)
}

fn at_places(file: &str, lines: &[u64]) -> Vec<(String, u64)> {
    lines.iter().map(|l| (file.to_string(), *l)).collect()
}

/// References of each kind of name, across the project's files and its
/// unsaved buffers; on an attribute path, the contributions to its cell.
#[test]
fn references_of_every_kind_of_name() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let network = root.join("network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.open(&network);

    // A predicate: its rule's head and the bodies that read it.
    let found = references(&mut c, &root, &stack, find(&stack, "vpc_peer_pair(ia", 2));
    assert_eq!(found, at_places("stacks/dform.df", &[78, 83, 90]));

    // An input: the stack's own reads, a component's and a module's.
    let found = references(&mut c, &root, &stack, find(&stack, "key env:", 4));
    for want in [
        ("stacks/dform.df".to_string(), 9),
        ("stacks/dform.df".into(), 15),
        ("stacks/dform.df".into(), 54),
        ("stacks/dform.df".into(), 96),
        ("network.df".into(), 19),
        ("baseline.df".into(), 16),
    ] {
        assert!(found.contains(&want), "{want:?} in {found:?}");
    }

    // A component's input: declared in the component, read there, given
    // by each instance block.
    let found = references(&mut c, &root, &network, find(&network, "input vpc_net", 6));
    assert_eq!(
        found,
        vec![
            ("network.df".into(), 11),
            ("network.df".into(), 19),
            ("stacks/dform.df".into(), 51),
            ("stacks/dform.df".into(), 54),
        ]
    );

    // An object input read by its fields (R-38), and a type alias.
    let found = references(&mut c, &root, &stack, find(&stack, "input cidrs", 6));
    assert_eq!(found, at_places("stacks/dform.df", &[10, 51, 54, 93]));
    let found = references(&mut c, &root, &stack, find(&stack, "type environment", 5));
    assert_eq!(found, at_places("stacks/dform.df", &[9, 40]));

    // A component, a module used, a module the stack uses.
    let found = references(&mut c, &root, &network, find(&network, "component vpc", 10));
    assert_eq!(
        found,
        vec![
            ("network.df".into(), 9),
            ("stacks/dform.df".into(), 51),
            ("stacks/dform.df".into(), 54),
        ]
    );
    // Its output read in the stack; its resources' addresses from outside
    // are strings (H-16).
    let found = references(&mut c, &root, &stack, find(&stack, "use database", 4));
    // Its inputs given by the stack's `set` block are its too (R-38).
    assert_eq!(found, at_places("stacks/dform.df", &[17, 27, 28, 60]));
    let found = references(&mut c, &root, &stack, find(&stack, "use baseline", 6));
    assert_eq!(found, at_places("stacks/dform.df", &[45]));

    // A resource by its name in its component (from outside its address
    // is a string, `net.vpc["peer/vpc"]`).
    let found = references(&mut c, &root, &network, find(&network, "net.vpc vpc", 8));
    assert_eq!(
        found,
        vec![
            ("network.df".into(), 14),
            ("network.df".into(), 19),
            ("network.df".into(), 25),
            ("network.df".into(), 26),
        ]
    );

    // An unsaved buffer's reads count.
    let original = std::fs::read_to_string(&stack).unwrap();
    c.change(
        &stack,
        2,
        &format!("{original}\nextra(x) where vpc_peer_pair(x, _, _, _)\n"),
    );
    let found = references(&mut c, &root, &stack, find(&stack, "vpc_peer_pair(ia", 2));
    assert_eq!(found, at_places("stacks/dform.df", &[78, 83, 90, 111]));

    // An attribute path: every rule contributing to the cell, the
    // component's field and the module's.
    let found = references(&mut c, &root, &network, find(&network, "tags = { env", 1));
    assert_eq!(
        found,
        vec![("baseline.df".into(), 10), ("network.df".into(), 19),]
    );
    c.shutdown();
}

/// Apply a workspace edit's `changes` to the files on disk and, as an
/// editor would, to the server's buffers (the demo is ASCII: a UTF-16
/// column is a byte).
fn apply_edit(c: &mut Client, root: &Path, edit: &Value) {
    let prefix = uri(root) + "/";
    for (u, edits) in edit["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("{edit}"))
    {
        let file = root.join(u.strip_prefix(&prefix).unwrap());
        let mut text = std::fs::read_to_string(&file).unwrap();
        let offset = |text: &str, p: &Value| -> usize {
            let line = p["line"].as_u64().unwrap() as usize;
            let start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
            start + p["character"].as_u64().unwrap() as usize
        };
        let mut spans: Vec<(usize, usize, String)> = edits
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    offset(&text, &e["range"]["start"]),
                    offset(&text, &e["range"]["end"]),
                    e["newText"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        spans.sort_by_key(|s| std::cmp::Reverse(s.0));
        for (s, e, t) in spans {
            text.replace_range(s..e, &t);
        }
        std::fs::write(&file, &text).unwrap();
        c.change(&file, 100, &text);
    }
}

fn rename(c: &mut Client, file: &Path, at: (u32, u32), new: &str) -> Value {
    c.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri(file) },
            "position": { "line": at.0, "character": at.1 },
            "newName": new,
        }),
    )
}

/// The error a request answers with.
fn refused(c: &mut Client, method: &str, params: Value) -> String {
    c.next += 1;
    let id = c.next;
    c.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    loop {
        let m = c.recv();
        if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
            return m["error"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} was not refused: {m}"))
                .to_string();
        }
        c.notes.push(m);
    }
}

/// prepareRename offers names and refuses keywords, builtins, schema
/// types and what a provider owns; a rename rewrites every reference.
#[test]
fn prepare_rename_refuses_what_is_not_the_programs() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let network = root.join("network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    let prepare = |file: &Path, at: (u32, u32)| json!({ "textDocument": { "uri": uri(file) }, "position": { "line": at.0, "character": at.1 } });
    let ok = c.request(
        "textDocument/prepareRename",
        prepare(&stack, find(&stack, "compute.vm bastion", 12)),
    );
    assert_eq!(ok["placeholder"], "bastion", "{ok}");
    for (file, needle, ahead, why) in [
        (&stack, "resource compute.vm bastion", 2, "keyword"),
        (&stack, "inet.host(", 2, "builtin"),
        (&stack, "compute.vm bastion", 9, "schema type"),
        (&network, "cidr = vpc_net", 1, "attribute path"),
        (
            &stack,
            "data(\"zone\", \"us-test-1a\")",
            1,
            "dform's own relation",
        ),
        (&stack, "provider fake", 10, "provider's name"),
        (&stack, "vpc_peer_pair(ia, ib", 15, "variable"),
    ] {
        let params = prepare(file, find(file, needle, ahead));
        let e = refused(&mut c, "textDocument/prepareRename", params);
        assert!(e.contains(why), "{needle}: {e}");
    }
    let params = json!({
        "textDocument": { "uri": uri(&stack) },
        "position": { "line": find(&stack, "compute.vm bastion", 12).0, "character": 22 },
        "newName": "not",
    });
    let e = refused(&mut c, "textDocument/rename", params);
    assert!(e.contains("not a name"), "{e}");

    // A module input: its declaration, its read and both instance blocks;
    // and with no state, no `moved`.
    let edit = rename(
        &mut c,
        &network,
        find(&network, "input vpc_net", 6),
        "cidr_block",
    );
    apply_edit(&mut c, &root, &edit);
    let text = std::fs::read_to_string(&network).unwrap();
    assert!(text.contains("input cidr_block: inet") && text.contains("cidr = cidr_block"));
    let text = std::fs::read_to_string(&stack).unwrap();
    assert_eq!(
        text.matches("{ cidr_block = inet(cidrs").count(),
        2,
        "{text}"
    );
    let edit = rename(
        &mut c,
        &stack,
        find(&stack, "compute.vm bastion", 12),
        "jump",
    );
    assert!(
        !edit.to_string().contains("moved("),
        "no state, no moved: {edit}"
    );
    c.shutdown();
}

/// Renaming a resource, and a module's resource, that have state offers
/// a `moved` fact per address in the same edit: the next plan is a move.
#[test]
fn rename_of_a_resource_with_state_plans_as_a_move() {
    let (_s, root) = example("demo");
    let run = |args: &[&str]| {
        let out = common::dform()
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "dform {args:?}: {text}");
        text
    };
    run(&["apply", "--yes", "dform", "env=staging"]);
    let stack = root.join("stacks/dform.df");
    let network = root.join("network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);

    let edit = rename(
        &mut c,
        &stack,
        find(&stack, "compute.vm bastion", 12),
        "jump",
    );
    let text = edit.to_string();
    assert!(
        text.contains(r#"moved(compute.vm, \"bastion\", jump)"#),
        "{edit}"
    );
    apply_edit(&mut c, &root, &edit);
    let edit = rename(&mut c, &network, find(&network, "net.vpc vpc", 8), "net0");
    for i in ["main", "peer"] {
        let fact = format!(r#"moved(net.vpc, \"{i}/vpc\", net.vpc[\"{i}/net0\"])"#);
        assert!(edit.to_string().contains(&fact), "{fact} in {edit}");
    }
    apply_edit(&mut c, &root, &edit);
    c.shutdown();

    let written = std::fs::read_to_string(&stack).unwrap();
    assert!(
        written.contains("resource compute.vm jump {")
            && written.contains("\"peer/net0\" in net.vpc"),
        "{written}"
    );
    let plan = run(&["plan", "dform", "env=staging"]);
    assert!(
        plan.contains("moved compute.vm[\"bastion\"] -> compute.vm[\"jump\"]"),
        "{plan}"
    );
    assert!(
        plan.contains("moved net.vpc[\"main/vpc\"] -> net.vpc[\"main/net0\"]"),
        "{plan}"
    );
    assert!(plan.contains("stack dform is undeformed"), "{plan}");
}

/// A rename that would change what the program means is refused: the
/// instance `main` is also the string `"main"` that `network.vpc[ia]` reads
/// (prepareRename names where), and renaming it anyway would lose the
/// peering (the checked evaluation says so).
#[test]
fn a_rename_that_changes_the_plan_is_refused() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    let at = find(&stack, "instance network.vpc main", 22);
    let e = refused(
        &mut c,
        "textDocument/prepareRename",
        json!({ "textDocument": { "uri": uri(&stack) }, "position": { "line": at.0, "character": at.1 } }),
    );
    assert!(
        e.contains(
            "instance main of component vpc is also the string \"main\" at stacks/dform.df:74:15"
        ),
        "{e}"
    );
    let e = refused(
        &mut c,
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri(&stack) },
            "position": { "line": at.0, "character": at.1 },
            "newName": "core",
        }),
    );
    assert!(
        e.contains("would no longer plan create net.vpc_peering[\"peer-main-peer\"]"),
        "{e}"
    );
    // Nothing was changed.
    let text = std::fs::read_to_string(&stack).unwrap();
    assert!(text.contains("instance network.vpc main {"));
    c.shutdown();
}

/// Two components' private relations of one name are two relations: a
/// rename of one leaves the other.
#[test]
fn same_named_private_relations_rename_independently() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let iam = root.join("identity.df");
    let k8s = root.join("kubernetes.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    for (file, value) in [(&iam, "iam"), (&k8s, "k8s")] {
        let text = std::fs::read_to_string(file).unwrap();
        let text = format!(
            "{text}\ncomponent {value}_part {{\n  helper(\"{value}\")\n  seen(x) where helper(x)\n}}\n"
        );
        std::fs::write(file, &text).unwrap();
        c.open(file);
    }
    let at = find(&iam, "helper(\"iam\")", 2);
    let line = find(&iam, "helper(\"iam\")", 0).0 as u64 + 1;
    let found = references(&mut c, &root, &iam, at);
    assert_eq!(found, at_places("identity.df", &[line, line + 1]));
    let edit = rename(&mut c, &iam, at, "iam_helper");
    let changes = edit["changes"].as_object().unwrap();
    assert_eq!(changes.len(), 1, "{edit}");
    assert_eq!(changes[&uri(&iam)].as_array().unwrap().len(), 2, "{edit}");
    apply_edit(&mut c, &root, &edit);
    let text = std::fs::read_to_string(&k8s).unwrap();
    assert!(text.contains("seen(x) where helper(x)"), "{text}");
    let text = std::fs::read_to_string(&iam).unwrap();
    assert!(text.contains("seen(x) where iam_helper(x)"), "{text}");
    c.shutdown();
}

/// The server's diagnostics are `dform plan`'s, also for a plan with a
/// replacement: the vpc is replaced, so the subnet that reads its id waits
/// on the new one (the program evaluated again without the replaced
/// object), and a policy denies what waits. Both refuse the same.
#[test]
fn diagnostics_of_a_plan_with_a_replacement_are_the_plans() {
    let s = common::Scratch::project("lsp-replace");
    let net = r#"

provider fake

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), tier = "web" }

deny(m) where deformation("pending", r, _), m = "${r} waits on a replacement"
"#;
    let file = s.write("stacks/p.df", net);
    s.run(&["apply", "p"]).success();
    std::fs::write(&file, net.replace("10.0.0.0/16", "10.1.0.0/16")).unwrap();
    let plan = s.run(&["plan", "p"]).failure();
    let refused: Vec<String> = plan
        .stderr
        .split("constraint violations:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("{}", plan.stderr))
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .map(str::to_string)
        .collect();
    assert_eq!(
        refused,
        ["net.subnet[\"a\"] waits on a replacement"],
        "{}{}",
        plan.stdout,
        plan.stderr
    );

    let root = std::fs::canonicalize(&s.dir).unwrap();
    let file = root.join("stacks/p.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&file);
    let ds = c.diagnostics(&file);
    let mut errors: Vec<String> = ds
        .iter()
        .filter(|d| d["severity"] == 1)
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect();
    errors.sort();
    assert_eq!(errors, refused, "{}", json!(ds));
    c.shutdown();
}

/// A deployment whose state is in a bucket is read there when the server
/// has the backend's credentials: the vpc applied there, and gone from the
/// program, is a delete that `prevent_destroy` denies, as `dform plan`
/// says. Without credentials it is evaluated as if nothing were deployed,
/// and says so.
#[test]
fn an_s3_deployment_is_read_with_credentials() {
    let server = dform_s3::fake::Server::start();
    let s = common::Scratch::project("lsp-s3");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[defaults]\nbackend = 's3(\"dform-test\", \"lsp/{{stack}}\", \
             {{endpoint: \"{}\", region: \"us-east-1\"}})'\n",
            server.endpoint
        ),
    );
    let spec = dform_core::store::S3Spec {
        bucket: "dform-test".into(),
        prefix: "lsp".into(),
        endpoint: Some(server.endpoint.clone()),
        region: Some("us-east-1".into()),
    };
    dform_s3::S3Store::with_credentials(&spec, "", rusty_s3::Credentials::new("fake", "fake"))
        .unwrap()
        .create_bucket()
        .unwrap();
    let vpc = "resource net.vpc main { cidr = \"10.0.0.0/16\" }\n";
    let net = format!("\n\nprovider fake\n\n{vpc}lifecycle(main, \"prevent_destroy\")\n");
    let file = s.write("stacks/p.df", &net);
    let creds = [
        ("DFORM_S3_ACCESS_KEY_ID", "fake"),
        ("DFORM_S3_SECRET_ACCESS_KEY", "fake"),
    ];
    let dform = |args: &[&str]| {
        let out = common::dform()
            .args(args)
            .current_dir(&s.dir)
            .envs(creds)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    };
    let (ok, text) = dform(&["apply", "--yes", "p"]);
    assert!(ok, "{text}");
    assert!(!s.path("dform.state/p/state.json").exists(), "{text}");
    // The resource goes; the fact names it by its address.
    let gone = net
        .replace(vpc, "")
        .replace("lifecycle(main", "lifecycle(net.vpc[\"main\"]");
    std::fs::write(&file, gone).unwrap();
    let deny = "lifecycle prevent_destroy: the plan would delete net.vpc[\"main\"]";
    let (ok, text) = dform(&["plan", "p"]);
    assert!(!ok && text.contains(&format!("- {deny}")), "{text}");

    let root = std::fs::canonicalize(&s.dir).unwrap();
    let file = root.join("stacks/p.df");
    let errors = |ds: &[Value]| -> Vec<String> {
        ds.iter()
            .filter(|d| d["severity"] == 1)
            .map(|d| d["message"].as_str().unwrap().to_string())
            .collect()
    };
    let mut with = common::dform();
    with.envs(creds);
    let mut c = Client::start_with(&root, json!({}), with);
    c.open(&file);
    let ds = c.diagnostics(&file);
    assert_eq!(errors(&ds), [deny], "{}", json!(ds));
    c.shutdown();

    let mut without = common::dform();
    for k in [
        "DFORM_S3_ACCESS_KEY_ID",
        "DFORM_S3_SECRET_ACCESS_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        without.env_remove(k);
    }
    let mut c = Client::start_with(&root, json!({}), without);
    c.open(&file);
    let ds = c.diagnostics(&file);
    assert!(errors(&ds).is_empty(), "{}", json!(ds));
    assert!(
        messages(&ds)
            .iter()
            .any(|m| m.contains("evaluated as if nothing were deployed there")),
        "{}",
        json!(ds)
    );
    c.shutdown();
}

/// A quantity literal's hover is its type, canonical form and base value
/// (R-66); `500m` shows both readings, the position deciding.
#[test]
fn hover_on_a_quantity_shows_its_base_value() {
    let (_s, root) = example("crud-api");
    let stack = root.join("stacks/crud_api.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    let text = |c: &mut Client, needle: &str| {
        let hover = c.at("textDocument/hover", &stack, find(&stack, needle, 1));
        hover["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let mem = text(&mut c, "512Mi");
    assert!(mem.contains("`bytes`: 512Mi = 536870912 bytes"), "{mem}");
    let cpu = text(&mut c, "500m");
    assert!(
        cpu.contains("in a cpu position, `cpu`: 500m = 500 millicores"),
        "{cpu}"
    );
    assert!(
        cpu.contains("in a duration position, `duration`: 8h20m"),
        "{cpu}"
    );
    c.shutdown();
}

/// Completion of an object input's field paths (R-54), after `k.` and
/// `k.f.`, and of a used module's relation outputs after `m.` (R-55).
#[test]
fn completion_offers_input_fields_and_relation_outputs() {
    let s = common::Scratch::project("lsp-fields");
    s.write(
        "stacks/zones.df",
        "\nprovider fake\ndecl zone(name: string)\nzone(\"a\")\noutput zone\n",
    );
    let file = s.write(
        "stacks/app.df",
        "\ninput nodes { flavor: string = \"b2\", count: int = 1, pool: { min: int = 1 } }\n\
         provider fake\nuse stacks.zones\nset { nodes.count = 2 }\n",
    );
    let root = std::fs::canonicalize(&s.dir).unwrap();
    let file = std::fs::canonicalize(&file).unwrap();
    let mut c = Client::start(&root, json!({}));
    c.open(&file);
    let original = std::fs::read_to_string(&file).unwrap();
    let typed = format!("{original}set nodes.\nset nodes.pool.\nseen(z) where zones.\n");
    c.change(&file, 2, &typed);
    let n = typed.lines().count() as u32;
    let fields = labels(&c.at("textDocument/completion", &file, (n - 3, 10)));
    assert_eq!(fields, ["flavor", "count", "pool"], "{fields:?}");
    let pool = labels(&c.at("textDocument/completion", &file, (n - 2, 15)));
    assert_eq!(pool, ["min"], "{pool:?}");
    let rels = labels(&c.at("textDocument/completion", &file, (n - 1, 20)));
    assert_eq!(rels, ["zone"], "{rels:?}");
}
