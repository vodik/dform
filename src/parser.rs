use crate::ast::{Atom, Component, Constraint, Lit, Program, RuleStmt, Stmt};
use crate::value::{Term, Value};
use anyhow::{anyhow, bail, Context, Result};
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "src/dform.pest"]
struct DformParser;

pub fn parse_program(src: &str) -> Result<Program> {
    let mut pairs = DformParser::parse(Rule::program, src).context("parse")?;
    let program = pairs
        .next()
        .ok_or_else(|| anyhow!("missing program"))?;

    let mut statements = Vec::new();
    for pair in program.into_inner() {
        match pair.as_rule() {
            Rule::stmt => {
                let inner = pair.into_inner().next().unwrap();
                statements.push(parse_stmt(inner)?);
            }
            Rule::EOI => {}
            _ => {}
        }
    }
    Ok(Program { statements })
}

fn parse_stmt(pair: Pair<Rule>) -> Result<Stmt> {
    match pair.as_rule() {
        Rule::fact => Ok(Stmt::Fact(parse_atom(pair.into_inner().next().unwrap())?)),
        Rule::rule_stmt => {
            let mut it = pair.into_inner();
            let head = parse_atom(it.next().unwrap())?;
            let body = parse_body(it.next().unwrap().into_inner())?;
            Ok(Stmt::Rule(RuleStmt { head, body }))
        }
        Rule::constraint_stmt => {
            let mut it = pair.into_inner();
            let msg = parse_string_lit(it.next().unwrap())?;
            let body = parse_body(it.next().unwrap().into_inner())?;
            Ok(Stmt::Constraint(Constraint { message: msg, body }))
        }
        Rule::component_stmt => {
            let mut it = pair.into_inner();
            let comp = it.next().unwrap().as_str().to_string();
            let inst = it.next().unwrap().as_str().to_string();
            let mut body = Vec::new();
            for p in it {
                if p.as_rule() != Rule::stmt {
                    continue;
                }
                let inner = p.into_inner().next().unwrap();
                body.push(parse_stmt(inner)?);
            }
            Ok(Stmt::Component(Component { comp, inst, body }))
        }
        _ => bail!("unexpected stmt: {:?}", pair.as_rule()),
    }
}

fn parse_body(pairs: Pairs<Rule>) -> Result<Vec<Lit>> {
    let mut out = Vec::new();
    for pair in pairs {
        match pair.as_rule() {
            Rule::lit => {
                let inner = pair.into_inner().next().unwrap();
                out.push(parse_lit(inner)?);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn parse_lit(pair: Pair<Rule>) -> Result<Lit> {
    match pair.as_rule() {
        Rule::not_atom => {
            let atom = parse_atom(pair.into_inner().next().unwrap())?;
            Ok(Lit::Not(atom))
        }
        Rule::cmp => parse_cmp(pair),
        Rule::atom => Ok(Lit::Pos(parse_atom(pair)?)),
        _ => bail!("unexpected lit: {:?}", pair.as_rule()),
    }
}

fn parse_cmp(pair: Pair<Rule>) -> Result<Lit> {
    let mut it = pair.into_inner();
    let left = parse_term(it.next().unwrap())?;
    let op = it.next().unwrap();
    let right = parse_term(it.next().unwrap())?;
    match op.as_str() {
        "=" => Ok(Lit::Eq(left, right)),
        "!=" => Ok(Lit::Neq(left, right)),
        ">" => Ok(Lit::Gt(left, right)),
        ">=" => Ok(Lit::Ge(left, right)),
        "<" => Ok(Lit::Lt(left, right)),
        "<=" => Ok(Lit::Le(left, right)),
        other => bail!("unknown op: {other}"),
    }
}

fn parse_atom(pair: Pair<Rule>) -> Result<Atom> {
    let mut it = pair.into_inner();
    let pred = it.next().unwrap().as_str().to_string();
    let mut args = Vec::new();
    if let Some(args_pair) = it.next() {
        for t in args_pair.into_inner() {
            if t.as_rule() == Rule::term {
                args.push(parse_term(t)?);
            }
        }
    }
    Ok(Atom { pred, args })
}

fn parse_term(pair: Pair<Rule>) -> Result<Term> {
    let inner = if pair.as_rule() == Rule::term {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::string => Ok(Term::Val(Value::Str(parse_string_lit(inner)?))),
        Rule::int => Ok(Term::Val(Value::Int(inner.as_str().parse()?))),
        Rule::bool_lit => Ok(Term::Val(Value::Bool(inner.as_str() == "true"))),
        Rule::var => Ok(Term::Var(inner.as_str().to_string())),
        Rule::ident => Ok(Term::Val(Value::Str(inner.as_str().to_string()))),
        Rule::func => {
            let mut it = inner.into_inner();
            let name = it.next().unwrap().as_str().to_string();
            let mut args = Vec::new();
            if let Some(args_pair) = it.next() {
                for t in args_pair.into_inner() {
                    if t.as_rule() == Rule::term {
                        args.push(parse_term(t)?);
                    }
                }
            }
            Ok(Term::Func { name, args })
        }
        _ => bail!("unexpected term: {:?}", inner.as_rule()),
    }
}

fn parse_string_lit(pair: Pair<Rule>) -> Result<String> {
    let s = pair.as_str();
    let unquoted = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .ok_or_else(|| anyhow!("bad string literal"))?;
    // Very small escape set for MVP.
    let mut out = String::new();
    let mut chars = unquoted.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next().ok_or_else(|| anyhow!("dangling escape"))? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            other => bail!("unsupported escape: \\{other}"),
        }
    }
    Ok(out)
}
