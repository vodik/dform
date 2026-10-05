//! The server loop: documents in, diagnostics out, requests answered from
//! the last evaluation. One thread: an edit marks its project dirty and
//! the project is evaluated again once edits pause for [`DEBOUNCE`]; a
//! request that reads an evaluation evaluates a dirty project first.

use crate::analysis::{self, Evaluated, Outcome, Problem, Severity, Target, Where};
use crate::{complete, explain, nav, refs, signature, text};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::RecvTimeoutError;
use dform_core::plugin::{Launch, Providers};
use dform_core::project::{self, Project};
use dform_core::schema::Schema;
use dform_core::syntax::SyntaxNode;
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CompletionParams, DiagnosticRelatedInformation, DiagnosticSeverity, DocumentFormattingParams,
    GotoDefinitionParams, HoverParams, Location, Position, Range, TextEdit, Uri,
};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long edits must pause before the project is evaluated again.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// The commands the server executes (editors/emacs/dform-ts-mode.el calls
/// both).
pub const SELECT_ENVIRONMENT: &str = "dform.selectEnvironment";
pub const WHY: &str = "dform.why";

/// How the server is run.
pub struct Options {
    /// The running dform's version, for dform.toml's requirement.
    pub version: &'static str,
    /// How to reach real providers, when the client opts in
    /// (`dform.lsp.real_providers`); otherwise the mock is linked in.
    pub real: &'static (dyn Launch + Sync),
}

/// The environment evaluations are of: the stacks' default key values, or
/// the named ones.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Env {
    #[default]
    Default,
    Keys(Vec<(String, String)>),
}

impl Env {
    fn label(&self) -> String {
        match self {
            Env::Default => "default".into(),
            Env::Keys(ks) => ks
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    /// `dform.selectEnvironment`'s argument: a map of key values,
    /// `k=v[,k=v]` typed as a string, or `default`.
    fn parse(arg: &Json) -> Result<Env> {
        match arg {
            Json::Null => Ok(Env::Default),
            Json::String(s) => {
                let s = s.trim();
                if s.is_empty() || s == "default" {
                    Ok(Env::Default)
                } else {
                    let mut keys = Vec::new();
                    for kv in s.split([',', ' ']).filter(|x| !x.is_empty()) {
                        let (k, v) = kv
                            .split_once('=')
                            .ok_or_else(|| anyhow!("{kv}: expected key=value"))?;
                        keys.push((k.to_string(), v.to_string()));
                    }
                    Ok(Env::Keys(keys))
                }
            }
            Json::Object(m) => {
                let m = match m.get("keys") {
                    Some(Json::Object(k)) => k,
                    _ => m,
                };
                let mut keys = Vec::new();
                for (k, v) in m {
                    let v = match v {
                        Json::String(s) => s.clone(),
                        v => v.to_string(),
                    };
                    keys.push((k.clone(), v));
                }
                Ok(if keys.is_empty() {
                    Env::Default
                } else {
                    Env::Keys(keys)
                })
            }
            other => Err(anyhow!("expected a map of key values, got {other}")),
        }
    }
}

/// One stack's last evaluation.
struct Evaluation {
    file: PathBuf,
    outcome: Outcome,
}

/// A project (or a stack file outside every project): its root, and its
/// stacks' last evaluations.
#[derive(Default)]
struct Workspace {
    stacks: Vec<Evaluation>,
    /// The files diagnostics were last published for.
    published: BTreeSet<PathBuf>,
}

struct Server<'c> {
    conn: &'c Connection,
    opts: Options,
    launch: &'static dyn Launch,
    /// Open documents, by canonical path.
    docs: BTreeMap<PathBuf, String>,
    env: Env,
    /// By root: a project's directory, or a loose stack file.
    workspaces: BTreeMap<PathBuf, Workspace>,
    dirty: BTreeSet<PathBuf>,
    deadline: Option<Instant>,
    /// Full provider schemas, for completion, by the providers' specs.
    schemas: BTreeMap<Vec<String>, Schema>,
    /// Every evaluation's wall-clock time (`dform/stats`).
    timings: Vec<Duration>,
    /// The requests sent the client.
    asked: u64,
}

/// Work in the workspace at `root`: the command line's names and paths are
/// relative to where it runs, the project's root (as `dform -C ROOT`), or
/// a loose stack file's directory.
fn enter(root: &Path) -> Result<()> {
    let dir = if root.is_dir() {
        root
    } else {
        root.parent().unwrap_or(Path::new("/"))
    };
    std::env::set_current_dir(dir).with_context(|| format!("cd {}", dir.display()))
}

/// Serve the client on stdin and stdout until it exits.
pub fn serve_stdio(opts: Options) -> Result<()> {
    let (conn, io) = Connection::stdio();
    serve(&conn, opts)?;
    drop(conn);
    io.join()?;
    Ok(())
}

/// Serve the client on `conn` until it exits.
pub fn serve(conn: &Connection, opts: Options) -> Result<()> {
    let (id, params) = conn.initialize_start()?;
    let init: lsp_types::InitializeParams = serde_json::from_value(params)?;
    let real = init
        .initialization_options
        .as_ref()
        .and_then(|o| {
            o.get("dform.lsp.real_providers")
                .or_else(|| o.pointer("/dform/lsp/real_providers"))
        })
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let launch: &'static dyn Launch = if real {
        opts.real
    } else {
        Box::leak(Box::new(dform_mock::Linked::direct()))
    };
    conn.initialize_finish(
        id,
        json!({
            "capabilities": {
                "textDocumentSync": { "openClose": true, "change": 1, "save": {} },
                "hoverProvider": true,
                "inlayHintProvider": true,
                "completionProvider": { "triggerCharacters": [".", " "] },
                "signatureHelpProvider": { "triggerCharacters": ["(", ",", "["] },
                "definitionProvider": true,
                "referencesProvider": true,
                "renameProvider": { "prepareProvider": true },
                "documentFormattingProvider": true,
                "codeActionProvider": { "codeActionKinds": ["quickfix"] },
                "executeCommandProvider": { "commands": [SELECT_ENVIRONMENT, WHY] },
            },
            "serverInfo": { "name": "dform", "version": opts.version },
        }),
    )?;
    let mut s = Server {
        conn,
        opts,
        launch,
        docs: BTreeMap::new(),
        env: Env::Default,
        workspaces: BTreeMap::new(),
        dirty: BTreeSet::new(),
        deadline: None,
        schemas: BTreeMap::new(),
        timings: Vec::new(),
        asked: 0,
    };
    s.run()
}

impl Server<'_> {
    fn run(&mut self) -> Result<()> {
        loop {
            let msg = match self.deadline {
                Some(d) => match self.conn.receiver.recv_deadline(d) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
                None => match self.conn.receiver.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return Ok(()),
                },
            };
            match msg {
                None => self.flush(),
                Some(Message::Request(req)) => {
                    if self.conn.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    let id = req.id.clone();
                    let method = req.method.clone();
                    // A request that panics fails alone; the session goes on.
                    let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.request(req)
                    }))
                    .unwrap_or_else(|p| {
                        let why = p
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| p.downcast_ref::<&str>().copied())
                            .unwrap_or("a panic");
                        Err(anyhow!("{method}: internal error: {why}"))
                    });
                    let resp = match answer {
                        Ok(v) => Response::new_ok(id, v),
                        Err(e) => Response::new_err(
                            id,
                            lsp_server::ErrorCode::RequestFailed as i32,
                            format!("{e:#}"),
                        ),
                    };
                    self.conn.sender.send(Message::Response(resp))?;
                }
                Some(Message::Notification(n)) => self.notification(n)?,
                Some(Message::Response(_)) => {}
            }
        }
    }

    fn notify(&self, method: &str, params: Json) {
        let _ = self
            .conn
            .sender
            .send(Message::Notification(Notification::new(
                method.into(),
                params,
            )));
    }

    /// Send the client a request; its answer is not waited for.
    fn ask(&mut self, method: &str, params: Json) {
        self.asked += 1;
        let id = lsp_server::RequestId::from(format!("dform-{}", self.asked));
        let _ = self
            .conn
            .sender
            .send(Message::Request(Request::new(id, method.into(), params)));
    }

    fn show(&self, message: impl Into<String>) {
        self.notify(
            "window/showMessage",
            json!({ "type": 3, "message": message.into() }),
        );
    }

    fn notification(&mut self, n: Notification) -> Result<()> {
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let p: lsp_types::DidOpenTextDocumentParams = serde_json::from_value(n.params)?;
                let path = self.path(&p.text_document.uri)?;
                self.docs.insert(path.clone(), p.text_document.text);
                self.touch(&path, false);
            }
            "textDocument/didChange" => {
                let p: lsp_types::DidChangeTextDocumentParams = serde_json::from_value(n.params)?;
                let path = self.path(&p.text_document.uri)?;
                // Full sync: the last change is the text.
                if let Some(c) = p.content_changes.into_iter().last() {
                    self.docs.insert(path.clone(), c.text);
                }
                self.touch(&path, true);
            }
            "textDocument/didSave" => {
                let p: lsp_types::DidSaveTextDocumentParams = serde_json::from_value(n.params)?;
                let path = self.path(&p.text_document.uri)?;
                self.touch(&path, false);
            }
            "textDocument/didClose" => {
                let p: lsp_types::DidCloseTextDocumentParams = serde_json::from_value(n.params)?;
                let path = self.path(&p.text_document.uri)?;
                self.docs.remove(&path);
                self.touch(&path, false);
            }
            _ => {}
        }
        Ok(())
    }

    fn request(&mut self, req: Request) -> Result<Json> {
        match req.method.as_str() {
            "textDocument/hover" => {
                let p: HoverParams = serde_json::from_value(req.params)?;
                let path = self.path(&p.text_document_position_params.text_document.uri)?;
                let root = self.root_of(&path);
                self.fresh(&root);
                let text = self.read(&path)?;
                let at = text::offset(&text, p.text_document_position_params.position);
                Ok(match explain::hover_at(&self.project(&root), &path, at) {
                    Some(value) => json!({ "contents": { "kind": "markdown", "value": value } }),
                    None => Json::Null,
                })
            }
            "textDocument/inlayHint" => {
                let p: lsp_types::InlayHintParams = serde_json::from_value(req.params)?;
                let path = self.path(&p.text_document.uri)?;
                let root = self.root_of(&path);
                self.fresh(&root);
                let text = self.read(&path)?;
                let range = (
                    text::offset(&text, p.range.start),
                    text::offset(&text, p.range.end),
                );
                let hints: Vec<Json> = crate::inlay::hints(&self.project(&root), &path, range)
                    .into_iter()
                    .map(|h| {
                        json!({
                            "position": text::position(&text, h.at),
                            "label": h.label,
                            "tooltip": h.tooltip,
                            "paddingLeft": true,
                        })
                    })
                    .collect();
                Ok(Json::Array(hints))
            }
            "textDocument/signatureHelp" => {
                let p: lsp_types::SignatureHelpParams = serde_json::from_value(req.params)?;
                let path = self.path(&p.text_document_position_params.text_document.uri)?;
                let text = self.read(&path)?;
                let at = text::offset(&text, p.text_document_position_params.position);
                let trees: Vec<SyntaxNode> = self
                    .files(&self.root_of(&path))
                    .iter()
                    .filter_map(|f| Some(self.tree(f)?.1))
                    .collect();
                Ok(serde_json::to_value(signature::help(&text, at, &trees))?)
            }
            "textDocument/completion" => {
                let p: CompletionParams = serde_json::from_value(req.params)?;
                let path = self.path(&p.text_document_position.text_document.uri)?;
                let items = self.complete(&path, p.text_document_position.position)?;
                Ok(serde_json::to_value(items)?)
            }
            "textDocument/definition" => {
                let p: GotoDefinitionParams = serde_json::from_value(req.params)?;
                let path = self.path(&p.text_document_position_params.text_document.uri)?;
                let locs = self.definition(&path, p.text_document_position_params.position)?;
                Ok(serde_json::to_value(locs)?)
            }
            "textDocument/formatting" => {
                let p: DocumentFormattingParams = serde_json::from_value(req.params)?;
                let path = self.path(&p.text_document.uri)?;
                let text = self.read(&path)?;
                // With the project's schemas, as `dform fmt` formats it.
                let formatted = dform_core::project::format_file(&path, &text, self.opts.version)
                    .map_err(|e| anyhow!("{e:#}"))?;
                if formatted == text {
                    return Ok(json!([]));
                }
                Ok(serde_json::to_value(vec![TextEdit {
                    range: text::range(&text, 0, text.len()),
                    new_text: formatted,
                }])?)
            }
            "textDocument/codeAction" => {
                let p: lsp_types::CodeActionParams = serde_json::from_value(req.params)?;
                self.code_actions(p)
            }
            "workspace/executeCommand" => {
                let p: lsp_types::ExecuteCommandParams = serde_json::from_value(req.params)?;
                let arg = p.arguments.into_iter().next();
                match p.command.as_str() {
                    SELECT_ENVIRONMENT => self.select_environment(arg),
                    WHY => self.why(arg),
                    c => Err(anyhow!("no command {c}")),
                }
            }
            "textDocument/references" | "textDocument/prepareRename" | "textDocument/rename" => {
                self.names(&req.method, req.params)
            }
            "dform/stats" => {
                let ms: Vec<f64> = self
                    .timings
                    .iter()
                    .map(|d| d.as_secs_f64() * 1000.0)
                    .collect();
                Ok(json!({ "evaluations": ms.len(), "ms": ms }))
            }
            m => Err(anyhow!("unsupported request {m}")),
        }
    }

    /// A document's canonical path.
    fn path(&self, uri: &Uri) -> Result<PathBuf> {
        let p = text::path_of(uri).ok_or_else(|| anyhow!("not a file URI: {}", uri.as_str()))?;
        Ok(std::fs::canonicalize(&p).unwrap_or(p))
    }

    /// A file's text: its open buffer, else the disk.
    fn read(&self, path: &Path) -> Result<String> {
        match self.docs.get(path) {
            Some(t) => Ok(t.clone()),
            None => {
                std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
            }
        }
    }

    /// The workspace a file belongs to: its project's root, else the file.
    fn root_of(&self, path: &Path) -> PathBuf {
        match project::manifest_root(path) {
            Some(r) => std::fs::canonicalize(&r).unwrap_or(r),
            None => path.to_path_buf(),
        }
    }

    fn touch(&mut self, path: &Path, debounce: bool) {
        self.dirty.insert(self.root_of(path));
        if debounce {
            self.deadline = Some(Instant::now() + DEBOUNCE);
        } else {
            self.flush();
        }
    }

    /// Evaluate every dirty workspace and publish what it found.
    fn flush(&mut self) {
        self.deadline = None;
        for root in std::mem::take(&mut self.dirty) {
            self.evaluate(&root);
            self.publish(&root);
        }
    }

    /// Evaluate the workspace at `root` if an edit is pending.
    fn fresh(&mut self, root: &Path) {
        if self.dirty.contains(root) || !self.workspaces.contains_key(root) {
            self.dirty.insert(root.to_path_buf());
            self.flush();
        }
    }

    /// The stacks of the workspace at `root`: discovery's, or the loose
    /// file.
    fn stacks(&self, root: &Path) -> Vec<(PathBuf, Vec<String>)> {
        if enter(root).is_err() {
            return Vec::new();
        }
        if root.is_dir() {
            let Ok(Some(project)) = Project::find(root, self.opts.version) else {
                return Vec::new();
            };
            return project::discover(&project)
                .stacks
                .into_iter()
                .map(|s| (root.join(&s.file), s.keys))
                .collect();
        }
        let Ok(text) = self.read(root) else {
            return Vec::new();
        };
        // A file is a stack named after itself (R-29).
        let parse = dform_core::syntax::parser::parse(&text);
        let keys = dform_core::syntax::resolve::key_names(&parse.syntax());
        vec![(root.to_path_buf(), keys)]
    }

    fn evaluate(&mut self, root: &Path) {
        if let Err(e) = enter(root) {
            self.show(format!("dform: {e:#}"));
            return;
        }
        let stacks = self.stacks(root);
        let docs = self.docs.clone();
        let read = move |p: &Path| -> std::io::Result<String> {
            match docs.get(p) {
                Some(t) => Ok(t.clone()),
                None => std::fs::read_to_string(p),
            }
        };
        let mut evaluations = Vec::new();
        for (file, _) in stacks {
            let target = Target {
                file: file.clone(),
                keys: match &self.env {
                    Env::Keys(ks) => ks.clone(),
                    _ => Vec::new(),
                },
            };
            let start = Instant::now();
            let outcome = analysis::evaluate(&target, self.launch, &read, self.opts.version);
            let took = start.elapsed();
            self.timings.push(took);
            self.notify(
                "window/logMessage",
                json!({ "type": 4, "message": format!(
                    "evaluated {} in {:.1} ms", file.display(), took.as_secs_f64() * 1000.0
                ) }),
            );
            evaluations.push(Evaluation { file, outcome });
        }
        let ws = self.workspaces.entry(root.to_path_buf()).or_default();
        ws.stacks = evaluations;
    }

    /// Publish the workspace's problems, and clear the files that had some
    /// and have none now. An open file no evaluation read gets its syntax
    /// errors.
    fn publish(&mut self, root: &Path) {
        let Some(ws) = self.workspaces.get(root) else {
            return;
        };
        let mut by_file: BTreeMap<PathBuf, Vec<lsp_types::Diagnostic>> = BTreeMap::new();
        for ev in &ws.stacks {
            for p in &ev.outcome.problems {
                let Some((file, range)) = self.locate(&p.at, &ev.file) else {
                    continue;
                };
                let d = self.diagnostic(p, range);
                let list = by_file.entry(file).or_default();
                if !list
                    .iter()
                    .any(|x| x.range == d.range && x.message == d.message)
                {
                    list.push(d);
                }
            }
        }
        for (path, text) in &self.docs {
            if self.root_of(path) != root || by_file.contains_key(path) {
                continue;
            }
            let parse = dform_core::syntax::parser::parse(text);
            let list: Vec<lsp_types::Diagnostic> = parse
                .errors
                .iter()
                .map(|e| lsp_types::Diagnostic {
                    range: text::range(text, e.start, e.end),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("dform".into()),
                    message: match &e.hint {
                        Some(h) => format!("{}\nhelp: {h}", e.message),
                        None => e.message.clone(),
                    },
                    ..Default::default()
                })
                .collect();
            if !list.is_empty() {
                by_file.insert(path.clone(), list);
            }
        }
        // What had problems and has none now, and every open file of the
        // workspace, is published (empty) too: the evaluation is done.
        let old = std::mem::take(&mut self.workspaces.get_mut(root).expect("evaluated").published);
        let open = self.docs.keys().filter(|d| self.root_of(d) == root);
        let cleared: BTreeSet<&PathBuf> = old
            .iter()
            .chain(open)
            .filter(|f| !by_file.contains_key(*f))
            .collect();
        for f in cleared {
            self.notify(
                "textDocument/publishDiagnostics",
                json!({ "uri": text::uri_of(f), "diagnostics": [] }),
            );
        }
        for (f, ds) in &by_file {
            self.notify(
                "textDocument/publishDiagnostics",
                json!({ "uri": text::uri_of(f), "diagnostics": ds }),
            );
        }
        self.workspaces.get_mut(root).expect("evaluated").published = by_file.into_keys().collect();
    }

    fn diagnostic(&self, p: &Problem, range: Range) -> lsp_types::Diagnostic {
        let related: Vec<DiagnosticRelatedInformation> = p
            .related
            .iter()
            .filter_map(|(w, m)| {
                let (f, r) = self.locate(w, Path::new(""))?;
                Some(DiagnosticRelatedInformation {
                    location: Location::new(text::uri_of(&f), r),
                    message: m.clone(),
                })
            })
            .collect();
        lsp_types::Diagnostic {
            range,
            severity: Some(match p.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
            }),
            source: Some("dform".into()),
            message: p.message.clone(),
            related_information: (!related.is_empty()).then_some(related),
            ..Default::default()
        }
    }

    /// A problem's place as a file and range; `Top` is the stack's file's
    /// first line.
    fn locate(&self, w: &Where, stack: &Path) -> Option<(PathBuf, Range)> {
        match w {
            Where::Top => {
                if stack.as_os_str().is_empty() {
                    return None;
                }
                let text = self.read(stack).ok()?;
                let end = text.find('\n').unwrap_or(text.len());
                Some((stack.to_path_buf(), text::range(&text, 0, end)))
            }
            Where::Bytes(f, start, end) => {
                let text = self.read(f).ok()?;
                Some((f.clone(), text::range(&text, *start, *end)))
            }
            Where::Line(f, line, col) => {
                let text = self.read(f).ok()?;
                Some((f.clone(), text::line_range(&text, *line, *col)))
            }
            Where::Span(_) | Where::Place(_) => None,
        }
    }

    /// The project at `root` as references and the hover read it: its
    /// files as the editor has them, its stacks' evaluations.
    fn project(&self, root: &Path) -> refs::Project<'_> {
        let dir = if root.is_dir() {
            root.to_path_buf()
        } else {
            root.parent().unwrap_or(Path::new("/")).to_path_buf()
        };
        let files: Vec<(PathBuf, String)> = self
            .files(root)
            .into_iter()
            .filter_map(|f| Some((f.clone(), self.read(&f).ok()?)))
            .collect();
        let evaluated: Vec<&Evaluated> = self.workspaces.get(root).map_or(Vec::new(), |w| {
            w.stacks
                .iter()
                .filter_map(|ev| ev.outcome.evaluated.as_ref())
                .collect()
        });
        let schemas = self.schema_files(&dir, &files, &evaluated);
        refs::Project {
            dir,
            files,
            evaluated,
            schemas,
        }
    }

    /// Each provider's schema file, read: a path, the project's
    /// providers/NAME/schema.df, a built-in schema extracted to the cache,
    /// a plugin's `schema.df` beside it. The providers are the
    /// evaluations', and the `provider` blocks' by the manifest (an
    /// evaluation that failed has none).
    fn schema_files(
        &self,
        dir: &Path,
        files: &[(PathBuf, String)],
        evaluated: &[&Evaluated],
    ) -> Vec<(PathBuf, Arc<Schema>)> {
        use dform_core::plugin::source::{Source, resolve};
        let manifest = Project::find(dir, self.opts.version).ok().flatten();
        let mut specs: BTreeSet<String> = evaluated
            .iter()
            .flat_map(|e| e.providers.iter().cloned())
            .collect();
        for (_, text) in files {
            let tree = dform_core::syntax::parser::parse(text).syntax();
            for n in tree
                .descendants()
                .filter(|n| n.kind() == dform_core::syntax::SyntaxKind::PROVIDER)
            {
                if let Some(name) = dform_core::names::declared_name(&n) {
                    let name = name.text().to_string();
                    specs.insert(
                        manifest
                            .as_ref()
                            .and_then(|m| m.manifest.provider_source(&name))
                            .unwrap_or(name),
                    );
                }
            }
        }
        specs
            .iter()
            .filter_map(|spec| {
                let local = dir.join("providers").join(spec).join("schema.df");
                let f = match resolve(spec) {
                    Source::Plugin(exe) => exe.parent()?.join("schema.df"),
                    Source::Mock(f) if f.ends_with(".df") || f.contains('/') => PathBuf::from(f),
                    Source::Mock(_) if local.is_file() => local,
                    Source::Mock(name) => return builtin_schema(&name),
                };
                let f = if f.is_absolute() { f } else { dir.join(f) };
                let schema = Schema::load(&f).ok()?;
                Some((f, Arc::new(schema)))
            })
            .collect()
    }

    /// The evaluation that reads `path`, and the facts the code at `pos`
    /// is about.
    fn explain(&mut self, path: &Path, pos: Position) -> Result<Option<(&Evaluated, Vec<usize>)>> {
        let root = self.root_of(path);
        self.fresh(&root);
        let text = self.read(path)?;
        let at = text::offset(&text, pos);
        let Some(ws) = self.workspaces.get(&root) else {
            return Ok(None);
        };
        for ev in &ws.stacks {
            let Some(e) = &ev.outcome.evaluated else {
                continue;
            };
            let in_file = |id: u32| e.files.get(&id).is_some_and(|f| f == path);
            let facts = explain::targets(e, &in_file, at);
            if !facts.is_empty() {
                return Ok(Some((e, facts)));
            }
        }
        Ok(None)
    }

    /// `dform.why`: the derivation text, shown as a message; with
    /// `"document": true` (the "Explain" code action, R-20) written to a
    /// read-only file under the cache and shown with `window/showDocument`
    /// instead, as a document of its own.
    fn why(&mut self, arg: Option<Json>) -> Result<Json> {
        let arg = arg.ok_or_else(|| anyhow!("dform.why: expected a position"))?;
        let document = arg.get("document").and_then(Json::as_bool) == Some(true);
        let p: lsp_types::TextDocumentPositionParams = serde_json::from_value(arg)?;
        let path = self.path(&p.text_document.uri)?;
        let found = self
            .explain(&path, p.position)?
            .map(|(e, facts)| (e.deployment.clone(), explain::why_text(e, &facts)));
        match found {
            Some((deployment, t)) if document => {
                let name: String = deployment
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                let f = cached(&format!("why/{name}.txt"), &t)
                    .ok_or_else(|| anyhow!("dform.why: no cache directory to write to"))?;
                self.ask(
                    "window/showDocument",
                    json!({ "uri": text::uri_of(&f), "takeFocus": true }),
                );
                Ok(Json::String(t))
            }
            Some((_, t)) => {
                self.show(t.clone());
                Ok(Json::String(t))
            }
            None => {
                self.show("dform why: nothing is derived here");
                Ok(Json::Null)
            }
        }
    }

    /// Every stack's key values, for a picker.
    fn choices(&self) -> Vec<Json> {
        let mut out = vec![json!({ "label": "default" })];
        let mut seen = BTreeSet::new();
        let roots: BTreeSet<PathBuf> = self
            .workspaces
            .keys()
            .cloned()
            .chain(self.docs.keys().map(|p| self.root_of(p)))
            .collect();
        for root in roots {
            for (file, keys) in self.stacks(&root) {
                let read = |p: &Path| -> std::io::Result<String> {
                    match self.docs.get(p) {
                        Some(t) => Ok(t.clone()),
                        None => std::fs::read_to_string(p),
                    }
                };
                let Ok(values) = analysis::choices(&file, &keys, &read) else {
                    continue;
                };
                for kv in values {
                    if seen.insert(kv.clone()) {
                        let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
                        out.push(json!({ "label": kv, "keys": { k: v } }));
                    }
                }
            }
        }
        out
    }

    fn select_environment(&mut self, arg: Option<Json>) -> Result<Json> {
        let Some(arg) = arg else {
            return Ok(json!({ "current": self.env.label(), "choices": self.choices() }));
        };
        self.env = Env::parse(&arg)?;
        let roots: BTreeSet<PathBuf> = self
            .workspaces
            .keys()
            .cloned()
            .chain(self.docs.keys().map(|p| self.root_of(p)))
            .collect();
        self.dirty.extend(roots);
        self.flush();
        let deployments: Vec<String> = self
            .workspaces
            .values()
            .flat_map(|w| &w.stacks)
            .filter_map(|ev| ev.outcome.evaluated.as_ref().map(|e| e.deployment.clone()))
            .collect();
        let status = json!({ "label": self.env.label(), "deployments": deployments });
        self.notify("dform/environment", status.clone());
        self.show(format!(
            "dform: environment {}: {}",
            self.env.label(),
            deployments.join(", ")
        ));
        Ok(status)
    }

    /// The full schema of `providers` (completion offers every type, not
    /// only those the program names).
    fn schema(&mut self, providers: &[String]) -> Option<&Schema> {
        if !self.schemas.contains_key(providers) {
            let s = Providers::start(self.launch, providers, &Default::default())
                .ok()?
                .schema()
                .clone();
            self.schemas.insert(providers.to_vec(), s);
        }
        self.schemas.get(providers)
    }

    /// Every `.df` file of the workspace at `root`, open or not.
    fn files(&self, root: &Path) -> Vec<PathBuf> {
        let mut out: BTreeSet<PathBuf> = BTreeSet::new();
        if root.is_dir() && enter(root).is_ok() {
            // A manifest that does not load (one a newer dform wrote) is
            // the evaluation's error; its files are still the project's.
            let p = match Project::find(root, self.opts.version) {
                Ok(Some(p)) => p,
                _ => Project {
                    root: root.to_path_buf(),
                    manifest: Default::default(),
                },
            };
            for f in project::df_files(&p) {
                let abs = if f.is_absolute() { f } else { root.join(f) };
                out.insert(std::fs::canonicalize(&abs).unwrap_or(abs));
            }
        } else {
            out.insert(root.to_path_buf());
        }
        out.extend(
            self.docs
                .keys()
                .filter(|d| self.root_of(d) == root)
                .cloned(),
        );
        out.into_iter().collect()
    }

    fn tree(&self, path: &Path) -> Option<(String, SyntaxNode)> {
        let text = self.read(path).ok()?;
        let root = dform_core::syntax::parser::parse(&text).syntax();
        Some((text, root))
    }

    fn complete(&mut self, path: &Path, pos: Position) -> Result<Vec<lsp_types::CompletionItem>> {
        let root = self.root_of(path);
        self.fresh(&root);
        let (text, tree) = self
            .tree(path)
            .ok_or_else(|| anyhow!("no text for {}", path.display()))?;
        let at = text::offset(&text, pos);
        let evaluated = self
            .workspaces
            .get(&root)
            .and_then(|w| w.stacks.iter().find_map(|ev| ev.outcome.evaluated.as_ref()));
        let (providers, scoped) = match evaluated {
            Some(e) => (e.providers.clone(), Some(e.schema.clone())),
            None => (Vec::new(), None),
        };
        let schema = match self.schema(&providers) {
            Some(s) => s.clone(),
            None => scoped.unwrap_or_default(),
        };
        let files = self.files(&root);
        // An instance's component: an item some file declares, else the
        // file its path names (R-65).
        let modules = |path: &str| {
            let last = path.rsplit('.').next().unwrap_or(path);
            files
                .iter()
                .find_map(|f| {
                    let (_, t) = self.tree(f)?;
                    nav::module_interface(&t, Some(last))
                })
                .or_else(|| {
                    let (f, item) = nav::path_file(&root, path)?;
                    let (_, t) = self.tree(&f)?;
                    nav::module_interface(&t, item.as_deref())
                })
        };
        Ok(complete::complete(&tree, &text, at, &schema, &modules))
    }

    /// Go-to-definition (`refs::definition`): a function's signature line
    /// in the std files extracted to the cache, a provider type's line in
    /// its schema file.
    fn definition(&mut self, path: &Path, pos: Position) -> Result<Vec<Location>> {
        let root = self.root_of(path);
        self.fresh(&root);
        let text = self.read(path)?;
        let at = text::offset(&text, pos);
        let project = self.project(&root);
        let schema_files: Vec<PathBuf> = project.schemas.iter().map(|(f, _)| f.clone()).collect();
        Ok(refs::definition(&project, path, at, &cached, &schema_files))
    }

    /// The quick fixes of the diagnostics the client names (`actions`):
    /// each one's edits, as one formatted edit per file.
    fn code_actions(&mut self, p: lsp_types::CodeActionParams) -> Result<Json> {
        use crate::actions;
        let path = self.path(&p.text_document.uri)?;
        let root = self.root_of(&path);
        self.fresh(&root);
        enter(&root)?;
        let read = |f: &Path| -> std::io::Result<String> {
            match self.docs.get(f) {
                Some(t) => Ok(t.clone()),
                None => std::fs::read_to_string(f),
            }
        };
        let mut found = Vec::new();
        for ev in self.workspaces.get(&root).map_or(&[][..], |w| &w.stacks) {
            found.extend(actions::compiled(&ev.outcome));
            if let Some(e) = &ev.outcome.evaluated {
                found.extend(actions::evaluated(e, &ev.file, &read));
            }
        }
        let text = self.read(&path)?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for a in found {
            let fixes: Vec<lsp_types::Diagnostic> = p
                .context
                .diagnostics
                .iter()
                .filter(|d| {
                    d.message.contains(&a.message)
                        && a.at.as_ref().is_none_or(|(f, s, e)| {
                            *f == path && text::range(&text, *s, *e) == d.range
                        })
                })
                .cloned()
                .collect();
            if fixes.is_empty() || !seen.insert((a.title.clone(), format!("{:?}", a.edits))) {
                continue;
            }
            let mut changes = serde_json::Map::new();
            for (f, edits) in &a.edits {
                let t = self.read(f)?;
                let (s, e, new_text) = actions::apply(&f.display().to_string(), &t, edits);
                let edit = TextEdit {
                    range: text::range(&t, s, e),
                    new_text,
                };
                changes.insert(text::uri_of(f).as_str().to_string(), json!([edit]));
            }
            out.push(json!({
                "title": a.title,
                "kind": "quickfix",
                "diagnostics": fixes,
                "edit": { "changes": changes },
            }));
        }
        // "Explain": what is derived here, as `dform why` prints it, in a
        // document of its own (R-20).
        let position = p.range.start;
        if self.explain(&path, position)?.is_some() {
            out.push(json!({
                "title": "Explain (dform why)",
                "command": {
                    "title": "Explain (dform why)",
                    "command": WHY,
                    "arguments": [{
                        "textDocument": { "uri": p.text_document.uri },
                        "position": position,
                        "document": true,
                    }],
                },
            }));
        }
        Ok(Json::Array(out))
    }

    /// References, prepareRename and rename (`refs`, `rename`), over the
    /// project's files as the editor has them and its evaluations.
    fn names(&mut self, method: &str, params: Json) -> Result<Json> {
        use crate::rename;
        let p: lsp_types::TextDocumentPositionParams = serde_json::from_value(params.clone())?;
        let path = self.path(&p.text_document.uri)?;
        let root = self.root_of(&path);
        self.fresh(&root);
        let text = self.read(&path)?;
        let at = text::offset(&text, p.position);
        let ws = self.workspaces.get(&root);
        let project = self.project(&root);
        match method {
            "textDocument/references" => {
                let r: lsp_types::ReferenceParams = serde_json::from_value(params)?;
                let decl = r.context.include_declaration;
                Ok(serde_json::to_value(refs::references(
                    &project, &path, at, decl,
                ))?)
            }
            "textDocument/prepareRename" => Ok(match rename::prepare(&project, &path, at)? {
                Some((range, placeholder)) => {
                    json!({ "range": range, "placeholder": placeholder })
                }
                None => Json::Null,
            }),
            _ => {
                let r: lsp_types::RenameParams = serde_json::from_value(params)?;
                let renaming = rename::rename(&project, &path, at, &r.new_name)?;
                // The edit is checked: the selected deployment evaluated
                // with it applied to the buffers.
                let before: Vec<&Outcome> = ws.map_or(Vec::new(), |w| {
                    w.stacks.iter().map(|ev| &ev.outcome).collect()
                });
                let mut docs = self.docs.clone();
                docs.extend(renaming.texts());
                let after = self.evaluate_with(&root, docs);
                renaming.verify(&before, &after)?;
                renaming.edit()
            }
        }
    }

    /// Every stack of the workspace at `root` evaluated in the selected
    /// environment with `docs` for the open buffers, as `evaluate` does,
    /// keeping nothing.
    fn evaluate_with(&self, root: &Path, docs: BTreeMap<PathBuf, String>) -> Vec<Outcome> {
        if enter(root).is_err() {
            return Vec::new();
        }
        let read = move |p: &Path| -> std::io::Result<String> {
            match docs.get(p) {
                Some(t) => Ok(t.clone()),
                None => std::fs::read_to_string(p),
            }
        };
        self.stacks(root)
            .into_iter()
            .map(|(file, _)| {
                let target = Target {
                    file,
                    keys: match &self.env {
                        Env::Keys(ks) => ks.clone(),
                        _ => Vec::new(),
                    },
                };
                analysis::evaluate(&target, self.launch, &read, self.opts.version)
            })
            .collect()
    }
}

/// A built-in schema (`schema::BUILTINS`), read once, and the file it is
/// extracted to.
fn builtin_schema(name: &str) -> Option<(PathBuf, Arc<Schema>)> {
    use std::sync::{LazyLock, Mutex};
    type Read = BTreeMap<String, Option<(PathBuf, Arc<Schema>)>>;
    static READ: LazyLock<Mutex<Read>> = LazyLock::new(Default::default);
    let mut read = READ.lock().ok()?;
    read.entry(name.to_string())
        .or_insert_with(|| {
            let text = dform_core::schema::builtin(name)?;
            let schema = Schema::parse(text, name).ok()?;
            Some((
                cached(&format!("schemas/{name}.df"), text)?,
                Arc::new(schema),
            ))
        })
        .clone()
}

/// A file shipped inside dform (a signature file of `functions::SOURCES`,
/// a built-in schema) extracted read-only under the user's cache
/// directory at `rel`, so what it declares has a definition to go to
/// (DESIGN.org R-24: as rust-analyzer's `rust-src`). The directory is
/// named by the shipped files' digest: another dform extracts beside it.
fn cached(rel: &str, source: &str) -> Option<PathBuf> {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        let schemas = dform_core::schema::BUILTINS
            .iter()
            .filter_map(|n| Some((*n, dform_core::schema::builtin(n)?)));
        let all: String = dform_core::functions::SOURCES
            .iter()
            .copied()
            .chain(schemas)
            .map(|(f, t)| format!("{f}\n{t}"))
            .collect();
        let digest = dform_core::approval::sha256_hex(all.as_bytes());
        base.join("dform")
            .join(format!("shipped-{}", &digest[..16]))
    });
    let f = dir.join(rel);
    if std::fs::read_to_string(&f).ok().as_deref() != Some(source) {
        std::fs::create_dir_all(f.parent()?).ok()?;
        let _ = std::fs::remove_file(&f);
        std::fs::write(&f, source).ok()?;
        let mut perms = std::fs::metadata(&f).ok()?.permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&f, perms).ok()?;
    }
    Some(f)
}
