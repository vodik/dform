//! Shell completion: `dform completions SHELL` and its helper,
//! `dform __complete`.

use super::Outcome;
use super::args::{Shell, experimental};
use crate::loader;
use anyhow::Result;
use std::path::Path;

/// `dform completions SHELL`.
#[derive(Debug, Clone)]
pub(super) struct Completions {
    pub(super) shell: Shell,
}

impl Completions {
    pub(super) fn run(&self) -> Result<Outcome> {
        print!("{}", self.script());
        Ok(Outcome::Done)
    }

    /// `dform completions SHELL`: a script that completes commands, and asks
    /// `dform __complete` for targets.
    fn script(&self) -> String {
        let shell = self.shell;
        let commands = listed(COMMANDS).join(" ");
        match shell {
            Shell::Zsh => format!(
                "#compdef dform\n\
                 # dform completions zsh > \"${{fpath[1]}}/_dform\"\n\
                 _dform() {{\n\
                 \x20 if (( CURRENT == 2 )); then\n\
                 \x20   compadd -- {commands}\n\
                 \x20 else\n\
                 \x20   compadd -- ${{(f)\"$(dform __complete ${{words[2,CURRENT-1]}} 2>/dev/null)\"}}\n\
                 \x20 fi\n\
                 }}\n\
                 compdef _dform dform\n"
            ),
            Shell::Bash => format!(
                "# dform completions bash > /etc/bash_completion.d/dform\n\
                 _dform() {{\n\
                 \x20 local cur=${{COMP_WORDS[COMP_CWORD]}}\n\
                 \x20 if [ \"$COMP_CWORD\" -eq 1 ]; then\n\
                 \x20   COMPREPLY=($(compgen -W \"{commands}\" -- \"$cur\"))\n\
                 \x20 else\n\
                 \x20   COMPREPLY=($(compgen -W \"$(dform __complete \"${{COMP_WORDS[@]:1:COMP_CWORD-1}}\" 2>/dev/null)\" -- \"$cur\"))\n\
                 \x20 fi\n\
                 }}\n\
                 complete -F _dform dform\n"
            ),
            Shell::Fish => format!(
                "# dform completions fish > ~/.config/fish/completions/dform.fish\n\
                 complete -c dform -f -n '__fish_use_subcommand' -a '{commands}'\n\
                 complete -c dform -f -n 'not __fish_use_subcommand' -a '(dform __complete (commandline -opc)[2..-1])'\n"
            ),
        }
    }
}

/// `dform __complete WORDS...`.
#[derive(Debug, Clone)]
pub(super) struct Complete {
    pub(super) words: Vec<String>,
}

impl Complete {
    /// `dform __complete WORDS...`: the candidates for the word after WORDS
    /// (the command line without `dform`): a noun's subcommands, else stack
    /// names from discovery, the deployments with state, and after a stack's
    /// name the values of its key inputs' enum types.
    pub(super) fn run(&self) -> Result<Outcome> {
        let words = &self.words;
        let words: Vec<&str> = words
            .iter()
            .map(String::as_str)
            .filter(|w| !w.starts_with('-'))
            .collect();
        let out: Vec<String> = match words.as_slice() {
            [noun] if !subcommands(noun).is_empty() => listed(subcommands(noun))
                .iter()
                .map(|s| s.to_string())
                .collect(),
            _ => {
                let Some(project) =
                    crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?
                else {
                    return Ok(Outcome::Done);
                };
                let d = crate::project::discover(&project);
                let mut out: Vec<String> = Vec::new();
                match words.last().and_then(|w| d.named(w).first().copied()) {
                    Some(s) => out.extend(key_values(s)),
                    None => {
                        out.extend(d.stacks.iter().map(|s| s.name.clone()));
                        let root = project.state_root();
                        for s in d.stacks.iter().filter(|s| !s.keys.is_empty()) {
                            let Ok(entries) = std::fs::read_dir(root.join(&s.name)) else {
                                continue;
                            };
                            for e in entries.flatten() {
                                let seg = e.file_name().to_string_lossy().into_owned();
                                if seg.contains('=') && e.path().join("state.json").exists() {
                                    out.push(format!("{}[{seg}]", s.name));
                                }
                            }
                        }
                    }
                }
                out
            }
        };
        let mut out = out;
        out.sort();
        out.dedup();
        for c in out {
            println!("{c}");
        }
        Ok(Outcome::Done)
    }
}

/// The commands and subcommands only `DFORM_EXPERIMENTAL=1` lists.
const EXPERIMENTAL_COMMANDS: &[&str] = &["controller", "handover"];

/// The top-level commands, and each noun's subcommands.
const COMMANDS: &[&str] = &[
    "plan",
    "apply",
    "destroy",
    "why",
    "query",
    "diff",
    "test",
    "fmt",
    "doc",
    "log",
    "output",
    "stack",
    "state",
    "secrets",
    "provider",
    "controller",
    "completions",
    "init",
    "lsp",
    "dev",
];

fn subcommands(noun: &str) -> &'static [&'static str] {
    match noun {
        "stack" => &["list", "rekey", "handover", "unlock"],
        "state" => &["show", "forget-host", "mv"],
        "secrets" => &["list", "rotate", "cycle"],
        "provider" => &["check", "schema"],
        "controller" => &["run"],
        "log" => &["verify"],
        "completions" => &["zsh", "bash", "fish"],
        "dev" => &[
            "plan",
            "apply",
            "destroy",
            "why",
            "query",
            "diff",
            "test",
            "log",
            "controller",
            "strata",
            "graph",
            "eval",
            "show",
        ],
        _ => &[],
    }
}

/// `names` without the experimental ones, unless they are listed.
fn listed<'a>(names: &[&'a str]) -> Vec<&'a str> {
    names
        .iter()
        .copied()
        .filter(|n| experimental() || !EXPERIMENTAL_COMMANDS.contains(n))
        .collect()
}

/// `K=V` for each value of each key input with an enum type.
fn key_values(s: &crate::project::Found) -> Vec<String> {
    let Ok(program) = loader::load_program(std::slice::from_ref(&s.file)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for st in &program.statements {
        let crate::ast::Stmt::Input(i) = st else {
            continue;
        };
        if !i.key {
            continue;
        }
        if let crate::ast::TypeExpr::Apply(n, args) = &i.ty
            && n == "enum"
        {
            for a in args {
                if let crate::ast::TypeExpr::Str(v) | crate::ast::TypeExpr::Name(v) = a {
                    out.push(format!("{}={v}", i.name));
                }
            }
        }
    }
    out
}
