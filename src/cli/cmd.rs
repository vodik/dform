//! What a run does ([`Cmd`]): one command, its arguments parsed, and what
//! each step of a run asks of it.

use super::apply::Apply;
use super::complete::{Complete, Completions};
use super::controller_cmd::Controller;
use super::dev::{Effects, Eval, Graph, Show, Strata};
use super::explain::{Diff, Explain, Query, Why};
use super::plan::Plan;
use super::provider_cmd::{ProviderCheck, ProviderSchema};
use super::secrets::Secrets;
use super::source::{Doc, Fmt, Init};
use super::stack::{Handover, Rekey, StackList};
use super::state_cmd::{ForgetHost, Log, Output, StateMv, StateShow, Unlock};
use super::status::Status;
use super::test::Test;
use super::{Cli, Outcome, Refused};
use crate::{query, report};
use anyhow::Result;

/// What a run does.
#[derive(Debug, Clone)]
pub(super) enum Cmd {
    Eval(Eval),
    Plan(Plan),
    Test(Test),
    Apply(Apply),
    Query(Query),
    Why(Why),
    Diff(Diff),
    Explain(Explain),
    Show(Show),
    Strata(Strata),
    Effects(Effects),
    Fmt(Fmt),
    Doc(Doc),
    Graph(Graph),
    Controller(Controller),
    Secrets(Secrets),
    ForgetHost(ForgetHost),
    Log(Log),
    StackList(StackList),
    Rekey(Rekey),
    Handover(Handover),
    Unlock(Unlock),
    StateShow(StateShow),
    Output(Output),
    StateMv(StateMv),
    Status(Status),
    ProviderCheck(ProviderCheck),
    ProviderSchema(ProviderSchema),
    Init(Init),
    Completions(Completions),
    Complete(Complete),
}

/// How far a run goes before its command runs: each step a command needs
/// is the one before it and more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    /// No program: the command line, the project.
    Alone,
    /// The program, loaded and compiled, with no deployment.
    Program,
    /// The deployment located: its state, its audit log.
    Objects,
    /// The deployment evaluated.
    Evaluated,
}

impl Cmd {
    /// The step of a run this command runs at.
    pub(super) fn stage(&self) -> Stage {
        match self {
            Cmd::Controller(_)
            | Cmd::Handover(_)
            | Cmd::StackList(_)
            | Cmd::Init(_)
            | Cmd::Completions(_)
            | Cmd::Complete(_)
            | Cmd::ProviderCheck(_)
            | Cmd::ProviderSchema(_)
            | Cmd::Fmt(_)
            | Cmd::Doc(_) => Stage::Alone,
            Cmd::Test(_) | Cmd::Strata(_) | Cmd::Effects(_) => Stage::Program,
            Cmd::Graph(g) if g.strata() => Stage::Program,
            Cmd::Log(_)
            | Cmd::Unlock(_)
            | Cmd::StateShow(_)
            | Cmd::Output(_)
            | Cmd::ForgetHost(_)
            | Cmd::StateMv(_) => Stage::Objects,
            Cmd::Eval(_)
            | Cmd::Plan(_)
            | Cmd::Apply(_)
            | Cmd::Query(_)
            | Cmd::Why(_)
            | Cmd::Diff(_)
            | Cmd::Explain(_)
            | Cmd::Show(_)
            | Cmd::Graph(_)
            | Cmd::Secrets(_)
            | Cmd::Rekey(_)
            | Cmd::Status(_) => Stage::Evaluated,
        }
    }

    /// Controller mode's commands (R-41), which say so on every run.
    pub(super) fn experimental(&self) -> bool {
        matches!(self, Cmd::Controller(_) | Cmd::Handover(_))
    }

    /// `apply PLAN.json`: its run is checked once the file's inputs (its
    /// world) are read.
    pub(super) fn applies_plan_file(&self) -> bool {
        matches!(self, Cmd::Apply(a) if a.plan_file.is_some())
    }

    /// A plan or an apply: what prints a plan.
    pub(super) fn plans(&self) -> bool {
        matches!(self, Cmd::Plan(_) | Cmd::Apply(_))
    }

    /// `destroy` and `plan --destroy` (R-149): the plan against an empty
    /// wanted set.
    pub(super) fn destroys(&self) -> bool {
        match self {
            Cmd::Plan(p) => p.destroy,
            Cmd::Apply(a) => a.destroy,
            _ => false,
        }
    }

    /// What an apply's messages call it: `destroy` or `apply`.
    pub(super) fn verb(&self) -> &'static str {
        match self.destroys() {
            true => "destroy",
            false => "apply",
        }
    }

    /// `--new-master` (R-163).
    pub(super) fn new_master(&self) -> bool {
        match self {
            Cmd::Plan(p) => p.new_master,
            Cmd::Apply(a) => a.new_master,
            _ => false,
        }
    }

    /// How much each printed change says of why (R-79).
    pub(super) fn why(&self) -> report::Why {
        match self {
            Cmd::Plan(p) => p.why,
            Cmd::Apply(a) => a.why,
            _ => report::Why::None,
        }
    }

    /// What evaluates against providers starts the program's, and there is
    /// no default.
    pub(super) fn needs_provider(&self) -> bool {
        matches!(
            self,
            Cmd::Eval(_)
                | Cmd::Plan(_)
                | Cmd::Test(_)
                | Cmd::Apply(_)
                | Cmd::Query(_)
                | Cmd::Why(_)
                | Cmd::Show(_)
                | Cmd::Controller(_)
                | Cmd::Secrets(_)
        )
    }

    /// A run that writes the deployment's objects checks first that a
    /// bucket keeps the conditions of a write; a plan only reads.
    pub(super) fn writes_objects(&self) -> bool {
        matches!(
            self,
            Cmd::Apply(_) | Cmd::StateMv(_) | Cmd::Unlock(_) | Cmd::Rekey(_)
        ) || matches!(self, Cmd::Plan(p) if p.out.is_some())
    }

    /// A command that only reads or moves the deployment's objects needs
    /// its key, not the program's other inputs.
    pub(super) fn objects_only(&self) -> bool {
        matches!(
            self,
            Cmd::StateShow(_) | Cmd::Output(_) | Cmd::StateMv(_) | Cmd::Log(_) | Cmd::Unlock(_)
        )
    }

    /// What the deployment's master is read for: a plan, an apply, and what
    /// reads its secrets.
    pub(super) fn holds_master(&self) -> bool {
        self.plans() || self.only_reads_secrets()
    }

    /// What only reads the deployment's secrets: a query, a why, `secrets`.
    pub(super) fn only_reads_secrets(&self) -> bool {
        matches!(self, Cmd::Query(_) | Cmd::Why(_) | Cmd::Secrets(_))
    }

    /// query and why read the policy pass, so a deny over the plan can be
    /// asked for and explained; a plan prints what it would do, conflicts
    /// included (E §2.8: a conflict is a fact, not an abort), and then
    /// refuses. Any other run is blocked by a violation.
    pub(super) fn explains(&self) -> bool {
        matches!(
            self,
            Cmd::Query(_) | Cmd::Why(_) | Cmd::Diff(_) | Cmd::Explain(_) | Cmd::Secrets(_)
        )
    }

    /// What reads what the last apply derived (R-80): the plan's guardrail
    /// compares against it, and its policy pass reads it.
    pub(super) fn reads_last_apply(&self) -> bool {
        matches!(
            self,
            Cmd::Plan(_) | Cmd::Apply(_) | Cmd::Why(_) | Cmd::Query(_)
        )
    }

    /// A `query` or `why` pattern, which may ask for the whole schema.
    pub(super) fn pattern(&self) -> Option<&str> {
        match self {
            Cmd::Query(q) => Some(&q.pattern),
            Cmd::Why(w) => Some(&w.pattern),
            _ => None,
        }
    }

    /// Whether a violation of the program blocks the run before it plans;
    /// `secrets set` gives what a violation may say is missing.
    pub(super) fn blocking(&self) -> bool {
        !matches!(
            self,
            Cmd::Plan(_) | Cmd::Query(_) | Cmd::Why(_) | Cmd::Rekey(_) | Cmd::Status(_)
        ) && !matches!(self, Cmd::Secrets(Secrets::Set { .. }))
    }

    /// Whether the run makes the plan's policy pass.
    pub(super) fn policy(&self) -> bool {
        (self.explains() && !matches!(self, Cmd::Secrets(_))) || matches!(self, Cmd::Plan(_))
    }

    /// The violations that refuse a run, printed, and the refusal: a
    /// conflict as the plan's `conflicts` section says it (R-111), not its
    /// raw context; an apply's as its footer says it.
    pub(super) fn blocked(&self, violations: &[String], redact: &query::Redactor) -> Result<()> {
        if violations.is_empty() {
            return Ok(());
        }
        let (conflicts, rest): (Vec<&String>, Vec<&String>) =
            violations.iter().partition(|v| report::is_conflict(v));
        if !conflicts.is_empty() {
            eprintln!("conflicts");
            for v in &conflicts {
                let shown =
                    report::violation_conflict(v, redact, report::Why::Line, report::Style::PLAIN);
                eprint!(
                    "{}",
                    shown.unwrap_or_else(|| format!("- {}\n", redact.text(v)))
                );
            }
        }
        if !rest.is_empty() {
            eprintln!("constraint violations:");
            for v in &rest {
                eprintln!("- {}", report::violation_line(v, redact));
            }
        }
        Err(match self {
            Cmd::Apply(_) => Refused::apply(self.verb(), conflicts.len(), rest.len(), None),
            _ => Refused::new("blocked by constraints", conflicts.len(), rest.len()),
        }
        .into())
    }

    /// Run a command of [`Stage::Alone`].
    pub(super) fn run_alone(cli: Cli) -> Result<Outcome> {
        match &cli.cmd {
            Cmd::Controller(c) => c.clone().run(cli),
            Cmd::Handover(c) => c.run(&cli),
            Cmd::StackList(c) => c.run(&cli),
            Cmd::Init(c) => c.run(),
            Cmd::Completions(c) => c.run(),
            Cmd::Complete(c) => c.run(),
            Cmd::ProviderCheck(c) => c.run(),
            Cmd::ProviderSchema(c) => c.run(),
            Cmd::Fmt(c) => c.run(),
            Cmd::Doc(c) => c.run(&cli),
            c => unreachable!("{c:?} runs at a later stage"),
        }
    }
}

impl Cli {
    /// Outside a project only what writes no state runs: a plan (without a
    /// plan file), a query, a `dev` view, or a run whose state is a world
    /// fixture's (`dev --world`, beside the world file).
    pub(super) fn needs_project(&self) -> bool {
        match &self.cmd {
            Cmd::Apply(_) | Cmd::Controller(_) => self.world.is_none(),
            Cmd::Plan(p) if p.out.is_some() => self.world.is_none(),
            Cmd::Log(_)
            | Cmd::StackList(_)
            | Cmd::Rekey(_)
            | Cmd::Handover(_)
            | Cmd::Unlock(_)
            | Cmd::StateShow(_)
            | Cmd::Output(_)
            | Cmd::StateMv(_)
            | Cmd::Secrets(_)
            | Cmd::ForgetHost(_)
            | Cmd::Status(_) => true,
            _ => false,
        }
    }
}
