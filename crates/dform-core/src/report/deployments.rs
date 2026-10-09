//! A plan of several deployments (R-200): the path tree printed. Each
//! deployment is a header line, as a copy of a component is (R-191): its
//! mark, its full name (`stacks.platform[env=lab]`), where it is listed
//! and what it is (`never applied`, `up to date`, its own count); its
//! plan nested under it. One headline counts across the tree. The same
//! line serves an apply (R-206): the mark and the full name, then what
//! happened where the plan says where.

use super::labels::marker_of;
use super::style::{Paint, Style, kind_paint};
use super::tally::Tally;
use crate::provider::ActionKind;

/// One deployment in a tree of them: its header line, and its plan.
#[derive(Debug, Clone)]
pub struct Deployed {
    /// What its plan does to it: `+` never applied, `~` changed, `=` up to
    /// date, `-` removed from the project module.
    pub kind: ActionKind,
    /// Its full name, `stacks.platform[env=lab]`.
    pub name: String,
    /// Where it is listed (`project.df:4`), else its stack's file.
    pub site: String,
    /// What it is: `never applied, 1 change (1 create) over 1 tick`, `up to
    /// date`, `failed`.
    pub state: String,
    /// Its plan as a nested report prints it (no headline of its own);
    /// empty when nothing is under it.
    pub body: String,
}

impl Deployed {
    /// The header line, its columns aligned at `width` (the widest name).
    fn header(&self, width: usize, style: Style) -> String {
        let mark = marker_of(&self.kind);
        let mark = match kind_paint(&self.kind) {
            Some(p) => style.paint(p, mark),
            None => mark.to_string(),
        };
        let pad = " ".repeat(width.saturating_sub(self.name.chars().count()));
        let right: Vec<&str> = [self.site.as_str(), self.state.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        let name = style.paint(Paint::Bold, &self.name);
        format!(
            "{mark} {name}{pad}  {}",
            style.paint(Paint::Dim, &right.join("  "))
        )
        .trim_end()
        .to_string()
    }
}

/// The tree: the headline, then each deployment's header with its plan
/// nested under it, a blank line before each plan and after it.
pub fn render(tally: &Tally, nodes: &[Deployed], style: Style) -> String {
    let mut out = format!("{}\n", tally.text());
    let width = nodes
        .iter()
        .map(|n| n.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut open = true;
    for n in nodes {
        let body = n.body.trim_matches('\n');
        if open || !body.is_empty() {
            out.push('\n');
        }
        out.push_str(&n.header(width, style));
        out.push('\n');
        for line in body.lines() {
            match line.is_empty() {
                true => out.push('\n'),
                false => out.push_str(&format!("  {line}\n")),
            }
        }
        open = !body.is_empty();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: ActionKind, name: &str, state: &str, body: &str) -> Deployed {
        Deployed {
            kind,
            name: name.into(),
            site: "project.df:2".into(),
            state: state.into(),
            body: body.into(),
        }
    }

    #[test]
    fn a_deployment_up_to_date_is_one_line_and_a_plan_nests_under_its_header() {
        let tally = Tally {
            kinds: vec![("create", 1)],
            ..Tally::default()
        };
        let nodes = [
            node(
                ActionKind::Noop,
                "stacks.platform[env=lab]",
                "up to date",
                "",
            ),
            node(
                ActionKind::Create,
                "stacks.apps[env=lab]",
                "never applied",
                "\ntick 1  1 change\n  + net.vpc rec  stacks/apps.df:4\n",
            ),
        ];
        assert_eq!(
            render(&tally, &nodes, Style::PLAIN),
            "plan: 1 change (1 create)\n\n\
             = stacks.platform[env=lab]  project.df:2  up to date\n\n\
             + stacks.apps[env=lab]      project.df:2  never applied\n\
             \x20 tick 1  1 change\n\
             \x20   + net.vpc rec  stacks/apps.df:4\n"
        );
    }
}
