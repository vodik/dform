//! Every sample the READMEs show is lifted from an example, so the two
//! cannot drift: README.md's and each `examples/NAME/README.md`'s. A
//! block of a program, and a block that shows a command and what it
//! prints, carry a comment on the line above the fence that names where
//! it comes from:
//!
//! - `<!-- examples/shop/stacks/shop.df:5-18 -->`: the block is those lines
//!   of the file, byte for byte; several ranges (`1-2,5-6,30-39,51`) are
//!   taken in turn.
//! - `<!-- run in examples/shop -->`: each `$ dform ..` line of the block
//!   runs in a fresh copy of the project, in order, and prints the lines
//!   under it on stdout; a line `...` stands for any lines, none
//!   included. `after dform apply; ..` runs commands first that the block
//!   does not show, and `TYPE NAME is degraded: REASON` says so of an
//!   object in the mock's world. A `$` line that is not dform's alone (a
//!   redirect, a pipe, another tool) is not run and shows nothing.
//!
//! A comment, unlike a name in the fence's info string, renders as
//! nothing on GitHub and leaves the fence's language to the highlighter,
//! and `path:lines` reads as the compiler's own sites do. Any ```dform
//! block, and any block showing a `$` command, without one is an error.

use crate::common;
use crate::examples::words;
use common::{Scratch, copy_dir, repo};
use std::path::Path;

/// A fenced block of a README and the comment above it.
struct Block {
    /// The README, from the repository's root, and the fence's line.
    at: String,
    lang: String,
    marker: Option<String>,
    lines: Vec<String>,
}

/// The fenced blocks of `readme`.
fn blocks(name: &str, readme: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut open: Option<Block> = None;
    let mut above: Option<&str> = None;
    for (i, l) in readme.lines().enumerate() {
        if let Some(b) = &mut open {
            if l == "```" {
                out.push(open.take().unwrap());
            } else {
                b.lines.push(l.to_string());
            }
            continue;
        }
        if let Some(lang) = l.strip_prefix("```") {
            let marker = above
                .and_then(|a| a.strip_prefix("<!-- "))
                .and_then(|a| a.strip_suffix(" -->"))
                .map(str::to_string);
            open = Some(Block {
                at: format!("{name}:{}", i + 1),
                lang: lang.to_string(),
                marker,
                lines: Vec::new(),
            });
        }
        above = Some(l);
    }
    assert!(open.is_none(), "{name}: a fence is not closed");
    out
}

/// The READMEs: the repository's and each example's.
fn readmes() -> Vec<String> {
    let mut out = vec!["README.md".to_string()];
    let mut examples: Vec<String> = std::fs::read_dir(repo().join("examples"))
        .unwrap()
        .flatten()
        .map(|e| format!("examples/{}/README.md", e.file_name().to_string_lossy()))
        .filter(|r| repo().join(r).is_file())
        .collect();
    examples.sort();
    out.extend(examples);
    out
}

/// The lines `ranges` names of the file `path` (`1-2,5`), in turn.
fn lines_of(path: &str, ranges: &str) -> Vec<String> {
    let text = std::fs::read_to_string(repo().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
    let all: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for r in ranges.split(',') {
        let (a, b) = r.split_once('-').unwrap_or((r, r));
        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
        assert!(
            1 <= a && a <= b && b <= all.len(),
            "{path}:{r} is not lines of it"
        );
        out.extend(all[a - 1..b].iter().map(|l| l.to_string()));
    }
    out
}

/// Whether `got` is `want`, a line `...` of it any lines.
fn matches(want: &[String], got: &[&str]) -> bool {
    match want.split_first() {
        None => got.is_empty(),
        Some((w, rest)) if w == "..." => (0..=got.len()).any(|i| matches(rest, &got[i..])),
        Some((w, rest)) => got.first() == Some(&w.as_str()) && matches(rest, &got[1..]),
    }
}

/// `TYPE NAME is degraded: REASON`: the object's health in the mock's
/// world of whichever deployment holds it.
fn degrade(s: &Scratch, step: &str) -> bool {
    let Some((object, reason)) = step.split_once(" is degraded: ") else {
        return false;
    };
    let (typ, name) = object.split_once(' ').unwrap();
    let mut worlds = Vec::new();
    find_worlds(&s.path("dform.state"), &mut worlds);
    let mut found = false;
    for w in worlds {
        let mut world: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&w).unwrap()).unwrap();
        for o in world["resources"].as_object_mut().unwrap().values_mut() {
            if o["typ"] == typ && o["name"] == name {
                o["health"] = serde_json::json!({ "state": "degraded", "reason": reason });
                found = true;
            }
        }
        std::fs::write(&w, world.to_string()).unwrap();
    }
    assert!(found, "no {typ} {name} in the mock's world");
    true
}

fn find_worlds(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            find_worlds(&p, out);
        } else if p.file_name().is_some_and(|n| n == "remote.json") {
            out.push(p);
        }
    }
}

/// A `$ dform ..` line's arguments, unless it is not run.
fn runnable(command: &str) -> Option<Vec<String>> {
    let line = command.strip_prefix("dform ")?;
    if line.contains(" > ") || line.contains(" | ") {
        return None;
    }
    Some(words(line))
}

/// Run a block's commands in a copy of the project `marker` names and
/// compare what each prints with the lines under it.
fn run(b: &Block, marker: &str) {
    let (dir, after) = match marker.split_once(" after ") {
        Some((d, a)) => (d, a.split("; ").collect()),
        None => (marker, vec![]),
    };
    let s = Scratch::in_target("readme", dir.rsplit('/').next().unwrap());
    copy_dir(&repo().join(dir), &s.dir);
    for step in after {
        if degrade(&s, step) {
            continue;
        }
        let args = runnable(step).unwrap_or_else(|| panic!("{}: `{step}` is no step", b.at));
        s.run(&args).success();
    }
    let mut commands: Vec<(&str, Vec<String>)> = Vec::new();
    for l in &b.lines {
        match l.strip_prefix("$ ") {
            Some(c) => commands.push((c, Vec::new())),
            None => match commands.last_mut() {
                Some((_, under)) => under.push(l.clone()),
                None => panic!("{}: `{l}` is under no `$` line", b.at),
            },
        }
    }
    assert!(!commands.is_empty(), "{}: no `$` line", b.at);
    for (c, want) in commands {
        let Some(args) = runnable(c) else {
            assert!(
                want.is_empty(),
                "{}: `{c}` is not run, so shows nothing",
                b.at
            );
            continue;
        };
        let r = s.run(&args);
        let got: Vec<&str> = r.stdout.lines().collect();
        assert!(
            matches(&want, &got),
            "{}: `$ {c}` in {dir} prints\n{}\n(stderr)\n{}\nnot\n{}",
            b.at,
            r.stdout,
            r.stderr,
            want.join("\n")
        );
    }
}

fn check(readme: &str) {
    let text = std::fs::read_to_string(repo().join(readme)).unwrap();
    for b in blocks(readme, &text) {
        let shows = b.lines.iter().any(|l| l.starts_with("$ "));
        let Some(marker) = &b.marker else {
            assert!(
                b.lang != "dform" && !shows,
                "{}: the block says where it comes from in a comment above it, \
                 `<!-- examples/NAME/FILE:LINES -->` or `<!-- run in examples/NAME -->`",
                b.at
            );
            continue;
        };
        match marker.strip_prefix("run in ") {
            Some(run_in) => run(&b, run_in),
            None => {
                let (path, ranges) = marker
                    .rsplit_once(':')
                    .unwrap_or_else(|| panic!("{}: `{marker}` is no `PATH:LINES`", b.at));
                let want = lines_of(path, ranges);
                assert_eq!(b.lines, want, "{}: the block is not {path}:{ranges}", b.at);
            }
        }
    }
}

#[test]
fn every_readme_block_is_lifted_from_an_example() {
    for r in readmes() {
        check(&r);
    }
}

/// The README's leaks are compile errors, each at its line with the code
/// its comment names.
#[test]
fn the_readmes_leaks_are_errors() {
    let file = "tests/fixtures/readme/leaks.df";
    let s = Scratch::in_target("readme", "leaks");
    copy_dir(&repo().join("tests/fixtures/readme"), &s.dir);
    let r = s.run(&["plan", "leaks.df"]).failure();
    let text = std::fs::read_to_string(repo().join(file)).unwrap();
    for (i, l) in text.lines().enumerate() {
        let Some((_, code)) = l.split_once("# E") else {
            continue;
        };
        let code = format!("[E{}]", &code[..4]);
        let at = format!("leaks.df:{}  ", i + 1);
        assert!(
            r.stderr
                .lines()
                .zip(r.stderr.lines().skip(1))
                .any(|(e, site)| e.ends_with(&code) && site.trim_start().starts_with(&at)),
            "{file}:{}: no {code} there\n{}",
            i + 1,
            r.stderr
        );
    }
}
