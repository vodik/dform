use crate::ast::{
    ApplyPolicy, Atom, Component, ComponentDef, Constraint, Import, Lit, PolicyPack, Program,
    Decl, Extern, Settings, FieldAssign, FieldOp, Rank, Resource, RuleStmt, Stmt, Term, Unique, Use, When,
};
use crate::value::Value;
use anyhow::{anyhow, bail, Context, Result};
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use pest_derive::Parser;
use std::collections::BTreeMap;

#[derive(Parser)]
#[grammar = "src/dform.pest"]
struct DformParser;

fn is_kw(rule: Rule) -> bool {
    matches!(
        rule,
        Rule::DECL
            | Rule::COMPONENT
            | Rule::COMPONENT_DEF
            | Rule::USE
            | Rule::POLICY_PACK
            | Rule::APPLY_POLICY
            | Rule::WHEN
            | Rule::IMPORT
            | Rule::AS
            | Rule::UNIQUE
            | Rule::SETTINGS
            | Rule::RESOURCE
            | Rule::CONSTRAINT
            | Rule::EXTERN
            | Rule::NOT
            | Rule::IN
            | Rule::TRUE
            | Rule::FALSE
    )
}

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
        Rule::decl_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let pred = it.next().unwrap().as_str().to_string();
            let mut fields = Vec::new();
            if let Some(df) = it.next() {
                for p in df.into_inner() {
                    if p.as_rule() == Rule::ident {
                        fields.push(p.as_str().to_string());
                    }
                }
            }
            Ok(Stmt::Decl(Decl { pred, fields }))
        }
        Rule::extern_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let pred = it.next().unwrap().as_str().to_string();
            let arity: usize = it.next().unwrap().as_str().parse()?;
            Ok(Stmt::Extern(Extern { pred, arity }))
        }
        Rule::fact => Ok(Stmt::Fact(parse_atom(pair.into_inner().next().unwrap())?)),
        Rule::rule_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let head = parse_atom(it.next().unwrap())?;
            let body = parse_body(it.next().unwrap().into_inner())?;
            Ok(Stmt::Rule(RuleStmt { head, body }))
        }
        Rule::constraint_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let msg = parse_string_lit(it.next().unwrap())?;
            let body = parse_body(it.next().unwrap().into_inner())?;
            Ok(Stmt::Constraint(Constraint { message: msg, body }))
        }
        Rule::component_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
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
        Rule::component_def_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let name = it.next().unwrap().as_str().to_string();
            let mut body = Vec::new();
            for p in it {
                if p.as_rule() != Rule::stmt {
                    continue;
                }
                let inner = p.into_inner().next().unwrap();
                body.push(parse_stmt(inner)?);
            }
            Ok(Stmt::ComponentDef(ComponentDef { name, body }))
        }
        Rule::use_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let name = it.next().unwrap().as_str().to_string();
            let inst = it.next().unwrap().as_str().to_string();
            let mut params: Vec<(String, Term)> = Vec::new();
            let mut body: Option<Vec<Lit>> = None;

            for p in it {
                match p.as_rule() {
                    Rule::res_fields => {
                        let assigns = parse_res_fields(p)?;
                        for a in assigns {
                            if !matches!(a.op, FieldOp::Assign) {
                                bail!("use params do not support +=");
                            }
                            if a.rank.is_some() {
                                bail!("use params do not take a rank");
                            }
                            params.push((a.key, a.value));
                        }
                    }
                    Rule::body => {
                        body = Some(parse_body(p.into_inner())?);
                    }
                    _ => {}
                }
            }

            Ok(Stmt::Use(Use {
                name,
                inst,
                params,
                body,
            }))
        }
        Rule::policy_pack_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let name = it.next().unwrap().as_str().to_string();
            let mut body = Vec::new();
            for p in it {
                if p.as_rule() != Rule::stmt {
                    continue;
                }
                let inner = p.into_inner().next().unwrap();
                body.push(parse_stmt(inner)?);
            }
            Ok(Stmt::PolicyPack(PolicyPack { name, body }))
        }
        Rule::apply_policy_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let name = it.next().unwrap().as_str().to_string();
            Ok(Stmt::ApplyPolicy(ApplyPolicy { name }))
        }
        Rule::when_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let guard = parse_guard(it.next().unwrap())?;
            let mut body = Vec::new();
            for p in it {
                if p.as_rule() != Rule::stmt {
                    continue;
                }
                let inner = p.into_inner().next().unwrap();
                body.push(parse_stmt(inner)?);
            }
            Ok(Stmt::When(When { guard, body }))
        }
        Rule::import_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let path = parse_string_lit(it.next().unwrap())?;
            let alias = it.next().map(|p| p.as_str().to_string());
            Ok(Stmt::Import(Import { path, alias }))
        }
        Rule::unique_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let pred = it.next().unwrap().as_str().to_string();
            let key_arity: usize = it.next().unwrap().as_str().parse()?;
            Ok(Stmt::Unique(Unique { pred, key_arity }))
        }
        Rule::settings_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let env = parse_term(it.next().unwrap())?;
            let mut rank = None;
            let mut fields: Vec<FieldAssign> = Vec::new();
            let mut body: Option<Vec<Lit>> = None;
            for p in it {
                match p.as_rule() {
                    Rule::rank => rank = Some(parse_rank(p)?),
                    Rule::res_fields => fields = parse_res_fields(p)?,
                    Rule::body => body = Some(parse_body(p.into_inner())?),
                    _ => {}
                }
            }
            Ok(Stmt::Settings(Settings { env, rank, fields, body }))
        }
        Rule::resource_stmt => {
            let mut it = pair.into_inner().filter(|p| !is_kw(p.as_rule()));
            let typ = parse_term(it.next().unwrap())?;
            let name = parse_term(it.next().unwrap())?;

            let mut rank = None;
            let mut fields: Vec<FieldAssign> = Vec::new();
            let mut body: Option<Vec<Lit>> = None;

            for p in it {
                match p.as_rule() {
                    Rule::rank => rank = Some(parse_rank(p)?),
                    Rule::res_fields => {
                        fields = parse_res_fields(p)?;
                    }
                    Rule::body => {
                        body = Some(parse_body(p.into_inner())?);
                    }
                    _ => {}
                }
            }
            Ok(Stmt::Resource(Resource {
                typ,
                name,
                rank,
                fields,
                body,
            }))
        }
        _ => bail!("unexpected stmt: {:?}", pair.as_rule()),
    }
}

fn parse_guard(pair: Pair<Rule>) -> Result<Lit> {
    match pair.as_rule() {
        Rule::guard => {
            let inner = pair.into_inner().next().unwrap();
            parse_guard(inner)
        }
        Rule::not_atom => {
            let mut atom_pair = None;
            for p in pair.into_inner() {
                if is_kw(p.as_rule()) {
                    continue;
                }
                if p.as_rule() == Rule::atom {
                    atom_pair = Some(p);
                    break;
                }
            }
            let atom = parse_atom(atom_pair.ok_or_else(|| anyhow!("missing atom after not"))?)?;
            Ok(Lit::Not(atom))
        }
        Rule::cmp => parse_cmp(pair),
        Rule::atom => Ok(Lit::Pos(parse_atom(pair)?)),
        _ => bail!("unexpected guard: {:?}", pair.as_rule()),
    }
}

fn parse_body(pairs: Pairs<Rule>) -> Result<Vec<Lit>> {
    let mut out = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::lit {
            let inner = pair.into_inner().next().unwrap();
            out.push(parse_lit(inner)?);
        }
    }
    Ok(out)
}

fn parse_lit(pair: Pair<Rule>) -> Result<Lit> {
    match pair.as_rule() {
        Rule::in_lit => {
            let mut terms = Vec::new();
            for p in pair.into_inner() {
                if is_kw(p.as_rule()) {
                    continue;
                }
                if p.as_rule() == Rule::term {
                    terms.push(parse_term(p)?);
                }
            }
            if terms.len() != 2 {
                bail!("in literal expects two terms");
            }
            let item = terms.remove(0);
            let list = terms.remove(0);
            Ok(Lit::Pos(Atom {
                pred: "member".to_string(),
                args: vec![list, item],
                record: None,
            }))
        }
        Rule::not_in_lit => {
            let mut terms = Vec::new();
            for p in pair.into_inner() {
                if is_kw(p.as_rule()) {
                    continue;
                }
                if p.as_rule() == Rule::term {
                    terms.push(parse_term(p)?);
                }
            }
            if terms.len() != 2 {
                bail!("not in literal expects two terms");
            }
            let item = terms.remove(0);
            let list = terms.remove(0);
            Ok(Lit::Not(Atom {
                pred: "member".to_string(),
                args: vec![list, item],
                record: None,
            }))
        }
        Rule::not_atom => {
            let mut atom_pair = None;
            for p in pair.into_inner() {
                if is_kw(p.as_rule()) {
                    continue;
                }
                if p.as_rule() == Rule::atom {
                    atom_pair = Some(p);
                    break;
                }
            }
            let atom = parse_atom(atom_pair.ok_or_else(|| anyhow!("missing atom after not"))?)?;
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

    let Some(next) = it.next() else {
        return Ok(Atom {
            pred,
            args: vec![],
            record: None,
        });
    };

    match next.as_rule() {
        Rule::args => {
            let mut args = Vec::new();
            for t in next.into_inner() {
                if t.as_rule() == Rule::term {
                    args.push(parse_term(t)?);
                }
            }
            Ok(Atom {
                pred,
                args,
                record: None,
            })
        }
        Rule::fields => {
            let mut fields: BTreeMap<String, Term> = BTreeMap::new();
            for f in next.into_inner() {
                if f.as_rule() != Rule::field {
                    continue;
                }
                let mut fit = f.into_inner();
                let key = fit.next().unwrap().as_str().to_string();
                let val = parse_term(fit.next().unwrap())?;
                fields.insert(key, val);
            }
            Ok(Atom {
                pred,
                args: Vec::new(),
                record: Some(fields),
            })
        }
        _ => bail!("unexpected atom form: {:?}", next.as_rule()),
    }
}

fn parse_term(pair: Pair<Rule>) -> Result<Term> {
    let inner = if pair.as_rule() == Rule::term {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::expr | Rule::add_expr | Rule::mul_expr | Rule::unary_expr | Rule::primary => {
            parse_expr(inner)
        }
        Rule::list_comp => {
            let mut it = inner.into_inner();
            let item = parse_term(it.next().unwrap())?;
            let body_pair = it.next().unwrap();
            let body = parse_body(body_pair.into_inner())?;
            Ok(Term::ListComp {
                item: Box::new(item),
                body,
            })
        }
        Rule::list_lit => {
            let mut items = Vec::new();
            for p in inner.into_inner() {
                if p.as_rule() == Rule::term {
                    items.push(parse_term(p)?);
                }
            }
            Ok(Term::List(items))
        }
        Rule::obj_lit => {
            let mut m: BTreeMap<String, Term> = BTreeMap::new();
            for p in inner.into_inner() {
                if p.as_rule() != Rule::obj_field {
                    continue;
                }
                let mut it = p.into_inner();
                let kpair = it.next().unwrap();
                let key = match kpair.as_rule() {
                    Rule::ident | Rule::sym => kpair.as_str().to_string(),
                    Rule::string => parse_string_lit(kpair)?,
                    _ => bail!("bad object key"),
                };
                let v = parse_term(it.next().unwrap())?;
                m.insert(key, v);
            }
            Ok(Term::Obj(m))
        }
        Rule::string => Ok(Term::Val(Value::Str(parse_string_lit(inner)?))),
        Rule::int => Ok(Term::Val(Value::Int(inner.as_str().parse()?))),
        Rule::bool_lit => Ok(Term::Val(Value::Bool(inner.as_str() == "true"))),
        Rule::var => {
            let v = inner.as_str().to_string();
            if v == "_" {
                Ok(Term::Wildcard)
            } else {
                Ok(Term::Var(v))
            }
        }
        Rule::sym | Rule::ident => Ok(Term::Val(Value::Str(inner.as_str().to_string()))),
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

fn parse_expr(pair: Pair<Rule>) -> Result<Term> {
    match pair.as_rule() {
        Rule::expr => parse_expr(pair.into_inner().next().unwrap()),
        Rule::add_expr => {
            let mut it = pair.into_inner();
            let mut lhs = parse_expr(it.next().unwrap())?;
            while let Some(op) = it.next() {
                let rhs = parse_expr(it.next().unwrap())?;
                lhs = match op.as_str() {
                    "+" => Term::Func {
                        name: "add".to_string(),
                        args: vec![lhs, rhs],
                    },
                    "-" => Term::Func {
                        name: "sub".to_string(),
                        args: vec![lhs, rhs],
                    },
                    other => bail!("unknown add op: {other}"),
                };
            }
            Ok(lhs)
        }
        Rule::mul_expr => {
            let mut it = pair.into_inner();
            let mut lhs = parse_expr(it.next().unwrap())?;
            while let Some(op) = it.next() {
                let rhs = parse_expr(it.next().unwrap())?;
                lhs = match op.as_str() {
                    "*" => Term::Func {
                        name: "mul".to_string(),
                        args: vec![lhs, rhs],
                    },
                    "/" => Term::Func {
                        name: "div".to_string(),
                        args: vec![lhs, rhs],
                    },
                    "%" => Term::Func {
                        name: "mod".to_string(),
                        args: vec![lhs, rhs],
                    },
                    other => bail!("unknown mul op: {other}"),
                };
            }
            Ok(lhs)
        }
        Rule::unary_expr => {
            let mut it = pair.into_inner().peekable();
            let mut negs = 0usize;
            while let Some(p) = it.peek() {
                if p.as_str() == "-" {
                    negs += 1;
                    let _ = it.next();
                    continue;
                }
                break;
            }
            let primary = it
                .next()
                .ok_or_else(|| anyhow!("missing primary expr"))?;
            let mut t = parse_expr(primary)?;
            if negs % 2 == 1 {
                t = Term::Func {
                    name: "sub".to_string(),
                    args: vec![Term::Val(Value::Int(0)), t],
                };
            }
            Ok(t)
        }
        Rule::primary => {
            let mut it = pair.into_inner();
            let first = it.next().unwrap();
            match first.as_rule() {
                Rule::expr | Rule::add_expr | Rule::mul_expr | Rule::unary_expr | Rule::primary => {
                    // Parenthesized expression: primary = "(" ~ expr ~ ")"
                    parse_expr(first)
                }
                other => parse_term(first).with_context(|| format!("primary inner {other:?}")),
            }
        }
        // These will be handled by parse_term directly.
        Rule::func
        | Rule::obj_lit
        | Rule::list_comp
        | Rule::list_lit
        | Rule::string
        | Rule::int
        | Rule::bool_lit
        | Rule::var
        | Rule::sym
        | Rule::ident => parse_term(pair),
        other => bail!("unexpected expr: {other:?}"),
    }
}

fn parse_res_fields(pair: Pair<Rule>) -> Result<Vec<FieldAssign>> {
    let mut out = Vec::new();
    for p in pair.into_inner() {
        if p.as_rule() != Rule::res_field {
            continue;
        }
        let mut it = p.into_inner();
        let key = it.next().unwrap().as_str().to_string();
        let op_pair = it.next().unwrap();
        let op = match op_pair.as_str() {
            "=" => FieldOp::Assign,
            "+=" => FieldOp::Add,
            other => bail!("unknown field op: {other}"),
        };
        let value = parse_term(it.next().unwrap())?;
        let rank = it.next().map(parse_rank).transpose()?;
        out.push(FieldAssign { key, op, value, rank });
    }
    Ok(out)
}

fn parse_rank(pair: Pair<Rule>) -> Result<Rank> {
    let s = pair.as_str().trim_start_matches('@');
    Rank::parse(s).ok_or_else(|| anyhow!("unknown rank @{s}"))
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
