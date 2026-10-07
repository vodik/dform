//! A client of `dform lsp` over stdio, for tests/lsp.rs and
//! tests/lsp_walk.rs: JSON-RPC requests and notifications, and copies of
//! the examples to serve.

#![allow(dead_code)]

use crate::common::{Scratch, copy_dir, repo};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A client speaking JSON-RPC to a `dform lsp` process.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pub next: u64,
    /// Notifications received, in order.
    pub notes: Vec<Value>,
}

impl Client {
    pub fn start(root: &Path, options: Value) -> Client {
        Client::start_with(root, options, crate::common::dform())
    }

    /// A server run as `command` (its environment) says.
    pub fn start_with(root: &Path, options: Value, mut command: Command) -> Client {
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

    pub fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    pub fn recv(&mut self) -> Value {
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

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// The result of a request; notifications meanwhile are kept.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.try_request(method, params.clone())
            .unwrap_or_else(|e| panic!("{method} {params}: {e}"))
    }

    /// The result of a request, or its error.
    pub fn try_request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next += 1;
        let id = self.next;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let m = self.recv();
            if m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
                return match m.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(m["result"].clone()),
                };
            }
            self.notes.push(m);
        }
    }

    /// The next notification `method` whose params satisfy `pred`, waiting
    /// for it.
    pub fn wait(&mut self, method: &str, pred: impl Fn(&Value) -> bool) -> Value {
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
    pub fn diagnostics(&mut self, file: &Path) -> Vec<Value> {
        let u = uri(file);
        let p = self.wait("textDocument/publishDiagnostics", |p| p["uri"] == u);
        p["diagnostics"].as_array().unwrap().clone()
    }

    pub fn open(&mut self, file: &Path) {
        let text = std::fs::read_to_string(file).unwrap();
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri(file), "languageId": "dform", "version": 1, "text": text
            }}),
        );
    }

    pub fn change(&mut self, file: &Path, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri(file), "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    pub fn at(&mut self, method: &str, file: &Path, (line, character): (u32, u32)) -> Value {
        self.request(
            method,
            json!({
                "textDocument": { "uri": uri(file) },
                "position": { "line": line, "character": character },
            }),
        )
    }

    pub fn command(&mut self, command: &str, arguments: Value) -> Value {
        self.request(
            "workspace/executeCommand",
            json!({ "command": command, "arguments": arguments }),
        )
    }

    pub fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}

pub fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

/// A copy of the example project `name`, its path canonical (as the server
/// names files).
pub fn example(name: &str) -> (Scratch, PathBuf) {
    let s = Scratch::new(&format!("lsp-{name}"));
    let root = s.dir.join(name);
    copy_dir(&repo().join("examples").join(name), &root);
    let root = std::fs::canonicalize(&root).unwrap();
    (s, root)
}

/// The (line, character) of the first `needle` in `file`, plus `ahead`
/// characters.
pub fn find(file: &Path, needle: &str, ahead: u32) -> (u32, u32) {
    let text = std::fs::read_to_string(file).unwrap();
    let at = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {}", file.display()));
    let p = dform_lsp::text::position(&text, at);
    (p.line, p.character + ahead)
}

/// A project shaped as a real one: the shared values a module file
/// declares, a component in a file of components, a stack another stack
/// reads by its deployment, an object input, two resources of one name.
pub const MODULES: &[(&str, &str)] = &[
    (
        "dform.toml",
        "[project]\nname = \"modules\"\nedition = \"2026\"\n",
    ),
    (
        "config.df",
        r#"#| What every stack shares.
let base_domain: string = "example.org"
let region: string = "r1"
"#,
    ),
    (
        "databases.df",
        r#"#| One database for one application.
component postgres {
  #| How an application connects.
  type conn = { host: string, port: int }

  input name: string
  input subnet: ref(net.subnet)
  output conn: conn = { host: "${name}.svc", port: 5432 }

  resource db.postgres db { subnets = [subnet] }
}
"#,
    ),
    (
        "stacks/platform.df",
        r#"key env: enum("lab", "prod") = "lab"
input nodes { flavor: string = "small", count: int = 1 check 1 <= count <= 3 }
output vpc: net.vpc = main
output subnet: net.subnet = main

set { nodes.count = 2 } where env == "prod"

use config
use fake
let base: inet = "10.0.0.0/16"
resource net.vpc main { cidr = "${base}" }

resource net.subnet main {
  vpc = main
  cidr = "${inet.subnet(base, 8, nodes.count)}"
}

lifecycle(net.vpc["main"], "prevent_destroy") where env == "prod"
"#,
    ),
    (
        "stacks/apps.df",
        r#"key env: enum("lab", "prod") = "lab"

use config
use stacks.platform
use databases

use fake

resource databases.postgres app_db {
  name = "app-${config.region}"
  subnet = platform[env].subnet
}

resource compute.vm web {
  private_ip = inet.host("10.0.0.0/24", 10)
  tags = { host: app_db.conn.host, domain: config.base_domain }
}
"#,
    ),
    (
        "stacks/vendored.df",
        r#"use fake

#| Machines as a manifest lists them: a resource per document (R-126).
let manifest = [{ tags: { host: "a" } }, { tags: { host: "b" } }]

resource compute.vm "${d.tags.host}" = d where d in manifest

resource compute.vm pinned = { tags: { host: "pinned" } }
"#,
    ),
];

pub fn modules_project() -> (crate::common::Scratch, PathBuf) {
    let s = crate::common::Scratch::new("lsp-walk-modules");
    let root = s.dir.join("modules");
    for (f, text) in MODULES {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let root = std::fs::canonicalize(&root).unwrap();
    (s, root)
}
