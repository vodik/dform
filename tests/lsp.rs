//! `dform lsp` over stdio, on copies of examples/demo and examples/pngu:
//! the contributors hover and `dform.why`, diagnostics of the selected
//! environment (`dform.selectEnvironment`), schema completion, and the
//! free ones (parse diagnostics, formatting, go-to-definition). The server
//! evaluates read only: the copies gain no dform.state/.

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
        let mut child = Command::new(env!("CARGO_BIN_EXE_dform"))
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
    let line = text[..at].matches('\n').count() as u32;
    let col = (at - text[..at].rfind('\n').map_or(0, |i| i + 1)) as u32;
    (line, col + ahead)
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
    let network = root.join("modules/network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&network);
    let at = find(&network, "tags = { env: env", 1);
    let hover = c.at("textDocument/hover", &network, at);
    let text = hover["contents"]["value"].as_str().unwrap().to_string();
    assert!(
        text.contains("dform[env=staging] (env from its default)"),
        "{text}"
    );
    assert!(
        text.contains("**net.vpc network.main::vpc .tags**"),
        "{text}"
    );
    assert!(
        text.contains("**net.vpc network.peer::vpc .tags**"),
        "{text}"
    );
    assert!(text.contains("winning rank: normal"), "{text}");
    // Every contribution: the module's, and the policy pack's.
    assert!(
        text.contains("modules/network.df:15:5, module network instance main"),
        "{text}"
    );
    assert!(
        text.contains("policies/baseline.df:16:3, policy baseline"),
        "{text}"
    );
    assert!(text.contains("by Σattr"), "{text}");

    let why = c.command(
        "dform.why",
        json!([{ "textDocument": { "uri": uri(&network) }, "position": { "line": at.0, "character": at.1 } }]),
    );
    let why = why.as_str().unwrap();
    assert!(
        why.starts_with(
            "attr(\"net.vpc\", \"network.main::vpc\", \"tags\", {component: \"network\", env: \"staging\", team: \"platform\"})\n  by Σattr"
        ),
        "{why}"
    );
    // Shown to the user too (eglot discards a command's result).
    let shown = c.wait("window/showMessage", |_| true);
    assert_eq!(shown["message"].as_str(), Some(why));

    // An attr read in a rule's body (`a.cidr`, of a deny that does not
    // fire): the attributes it matches.
    let stdlib = root.join("modules/stdlib_net.df");
    c.open(&stdlib);
    let hover = c.at("textDocument/hover", &stdlib, find(&stdlib, "a.cidr", 3));
    let text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        text.contains("**net.vpc network.main::vpc .cidr** = `10.50.0.0/16`"),
        "{hover}"
    );
    assert!(
        text.contains("modules/network.df:14:5, module network instance peer"),
        "{hover}"
    );
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
    for want in [
        "default",
        "env=dev",
        "env=staging",
        "env=prod",
        "scenario prod",
        "scenario dev",
    ] {
        assert!(offered.contains(&want.to_string()), "{want} in {offered:?}");
    }

    let original = std::fs::read_to_string(&stack).unwrap();
    let edited = format!("{original}\ndeny \"prod is frozen\" if env == \"prod\"\n");
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
        .position(|l| l.starts_with("deny \"prod"))
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
        .position(|l| l.contains("private_ip = inet_host"))
        .unwrap() as u64;
    assert!(lines.contains(&added) && lines.contains(&own), "{conflict}");

    // A scenario: its facts and denies.
    let status = c.command("dform.selectEnvironment", json!(["dev"]));
    assert_eq!(status["label"], "scenario dev");

    // A syntax error, as typed.
    c.change(&stack, 4, &format!("{original}\nresource compute.vm {{\n"));
    let ds = c.diagnostics(&stack);
    assert!(ds.iter().any(|d| d["severity"] == 1), "{ds:?}");
    c.shutdown();
    assert!(!root.join("dform.state").exists(), "the server wrote state");
    drop(s);
}

/// Schema completion: a resource block's paths with their type, flags and
/// refinements; resource types; an instance's module inputs, and outputs
/// after `module.instance.`; grant patterns.
#[test]
fn completion_reads_the_schema_and_the_modules() {
    let (_s, root) = example("demo");
    let stack = root.join("stacks/dform.df");
    let database = root.join("modules/database.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.open(&database);

    let network = root.join("modules/network.df");
    c.open(&network);
    let items = c.at(
        "textDocument/completion",
        &network,
        find(&network, "cidr = vpc_net", 0),
    );
    let names = labels(&items);
    assert!(names.contains(&"cidr".to_string()), "{names:?}");
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

    // An instance block: its module's inputs.
    let at = find(&stack, "instance network main {", 0);
    let items = c.at("textDocument/completion", &stack, (at.0 + 1, 2));
    assert!(labels(&items).contains(&"vpc_net".to_string()), "{items}");

    // `resource `, `contributes `, `network.main.`, as typed.
    let original = std::fs::read_to_string(&stack).unwrap();
    let typed = format!("{original}\nresource \nx = network.main.\npolicy p {{ contributes \n}}\n");
    c.change(&stack, 2, &typed);
    let n = typed.lines().count() as u32;
    let types = labels(&c.at("textDocument/completion", &stack, (n - 4, 9)));
    assert!(types.contains(&"net.vpc".to_string()), "{types:?}");
    let outputs = labels(&c.at("textDocument/completion", &stack, (n - 3, 17)));
    assert_eq!(outputs, vec!["vpc", "private_subnet_ids"], "{outputs:?}");
    let grants = labels(&c.at("textDocument/completion", &stack, (n - 2, 23)));
    assert!(grants.contains(&"_.cidr".to_string()), "{grants:?}");
    assert!(grants.contains(&"net.vpc.cidr".to_string()), "{grants:?}");

    // In a policy pack, `r.`: the paths its grants allow.
    let baseline = root.join("policies/baseline.df");
    c.open(&baseline);
    let text = std::fs::read_to_string(&baseline).unwrap();
    let after = "contributes settings.audit.sinks\n";
    let i = text.find(after).unwrap() + after.len();
    let typed = format!("{}  r.\n{}", &text[..i], &text[i..]);
    c.change(&baseline, 2, &typed);
    let line = text[..i].matches('\n').count() as u32;
    let granted = labels(&c.at("textDocument/completion", &baseline, (line, 4)));
    assert_eq!(
        granted,
        vec!["tags", "statements", "audit.sinks"],
        "{granted:?}"
    );
    c.shutdown();
}

/// Go-to-definition of a module, a policy and a predicate; formatting by
/// `dform fmt`'s formatter.
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
    assert_eq!(
        def(&mut c, "instance network main", 10),
        vec![("modules/network.df".into(), 2)]
    );
    assert_eq!(
        def(&mut c, "apply baseline", 7),
        vec![("policies/baseline.df".into(), 2)]
    );
    let text = std::fs::read_to_string(&stack).unwrap();
    let head = text
        .lines()
        .position(|l| l.starts_with("vpc_peer_pair(ia"))
        .unwrap() as u64;
    assert_eq!(
        def(&mut c, "vpc_peer_pair(ia, ib, a, b)\n", 2),
        vec![("stacks/dform.df".into(), head)]
    );
    assert!(
        def(&mut c, "network[ia].vpc", 1).contains(&("modules/network.df".into(), 2)),
        "a module read by its instances"
    );

    let messy = "edition 2026\nq(1)\np(x)   if q(x)\n";
    let scratch = root.join("stacks/messy.df");
    std::fs::write(&scratch, messy).unwrap();
    c.open(&scratch);
    let edits = c.request(
        "textDocument/formatting",
        json!({ "textDocument": { "uri": uri(&scratch) }, "options": { "tabSize": 2, "insertSpaces": true } }),
    );
    assert_eq!(
        edits[0]["newText"], "edition 2026\nq(1)\np(x) if q(x)\n",
        "{edits}"
    );
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
    let network = root.join("modules/network.df");
    let mut c = Client::start(&root, json!({}));
    c.open(&stack);
    c.open(&network);
    c.diagnostics(&stack);
    let at = find(&network, "tags = { env: env", 1);
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
