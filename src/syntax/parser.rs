//! Recursive descent over the token stream into a lossless rowan tree
//! (docs/grammar.md). Statements are hand-written productions, terms a Pratt
//! parser. Names are parsed unresolved: a `CHAIN` is `name (.seg | [t] |
//! /name)*`, and what it denotes is the resolver's business
//! (`syntax::lower`).
//!
//! A newline outside `( )`, `[ ]` and the braces of an object ends a
//! statement, a block entry or a literal of a `{ }` body; a line that ends
//! with `,`, an operator or a keyword that needs more continues. An error
//! inside a statement abandons it: the rest of it, up to the next line
//! outside its brackets or the `}` that ends its block, becomes one ERROR
//! node, so one bad statement is one diagnostic and parsing goes on.

use super::SyntaxKind::{self, *};
use crate::lexer::{self, Token};
use rowan::{Checkpoint, GreenNode, GreenNodeBuilder};

/// A syntax error: a byte range and what was expected there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub hint: Option<String>,
}

pub struct Parse {
    pub green: GreenNode,
    pub errors: Vec<ParseError>,
}

impl Parse {
    pub fn syntax(&self) -> super::SyntaxNode {
        super::SyntaxNode::new_root(self.green.clone())
    }
}

/// Parse a whole file.
pub fn parse(src: &str) -> Parse {
    let mut p = Parser::new(src);
    p.start_root();
    p.stmts(false);
    p.finish_all();
    p.into_parse()
}

/// Parse one term, for an interpolation hole: the tree is a SOURCE_FILE
/// holding the term (and an ERROR node for anything after it).
pub fn parse_term(src: &str) -> Parse {
    let mut p = Parser::new(src);
    p.start_root();
    p.nl.push(false);
    if p.term().is_ok() && p.nth(0) != EOF {
        let msg = format!("expected the end of the hole, found {}", p.found());
        p.error_here(msg, None);
    }
    p.start(ERROR);
    while p.nth(0) != EOF {
        p.bump();
    }
    p.finish();
    p.finish_all();
    p.into_parse()
}

/// The end of input; never a real token.
const EOF: SyntaxKind = SyntaxKind::__LAST;

/// An abandoned statement. The error is already recorded.
struct Bail;
type P<T = ()> = Result<T, Bail>;

/// A name that can start a chain in a term: an identifier, or a keyword
/// that has no construct of its own in a term.
fn term_name(k: SyntaxKind) -> bool {
    k == IDENT
        || (k.is_keyword()
            && !matches!(
                k,
                NOT_KW
                    | IN_KW
                    | IF_KW
                    | FOR_KW
                    | HAS_KW
                    | SOME_KW
                    | EXISTS_KW
                    | TRUE_KW
                    | FALSE_KW
                    | NULL_KW
                    | WHERE_KW
            ))
}

/// Any word: a key, a path segment, a declared name.
fn word(k: SyntaxKind) -> bool {
    k == IDENT || k.is_keyword()
}

fn is_cmp(k: SyntaxKind) -> bool {
    matches!(k, EQ | EQ2 | NEQ | LT | LE | GT | GE)
}

/// Keywords that start a statement: error recovery resynchronises on a line
/// that starts with one in column 0.
fn stmt_keyword(k: SyntaxKind) -> bool {
    matches!(
        k,
        EDITION_KW
            | PROVIDER_KW
            | STACK_KW
            | IMPORT_KW
            | INPUT_KW
            | OUTPUT_KW
            | EXPORT_KW
            | CONTRIBUTES_KW
            | MODULE_KW
            | INSTANCE_KW
            | POLICY_KW
            | APPLY_KW
            | RESOURCE_KW
            | SETTINGS_KW
            | SCENARIO_KW
            | EXTERN_KW
            | TYPE_KW
            | DECL_KW
            | WHEN_KW
            | FOR_KW
            | LET_KW
            | DENY_KW
            | WARN_KW
            | CONSTRAINT_KW
    )
}

/// Statements of the old, Prolog-shaped surface.
fn old_spelling(word: &str) -> Option<&'static str> {
    Some(match word {
        "component_def" => "`component_def` is spelled `module`",
        "use" => "`use` is spelled `instance`",
        "component" => "a component is a `module` and an `instance` of it",
        "policy_pack" => "`policy_pack` is spelled `policy`",
        "apply_policy" => "`apply_policy` is spelled `apply`",
        "unique" => "`unique` is gone: one value per key is what the attribute aggregate enforces",
        _ => return None,
    })
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    /// Index of the next token, trivia included.
    pos: usize,
    builder: GreenNodeBuilder<'static>,
    depth: usize,
    errors: Vec<ParseError>,
    /// Whether a newline ends what is being parsed: true at statement level
    /// and in blocks and `{ }` bodies, false inside brackets.
    nl: Vec<bool>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            src,
            toks: lexer::lex(src),
            pos: 0,
            builder: GreenNodeBuilder::new(),
            depth: 0,
            errors: Vec::new(),
            nl: vec![true],
        }
    }

    fn into_parse(self) -> Parse {
        Parse {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    // --- token access -----------------------------------------------------

    /// Index into `toks` of the `n`th non-trivia token ahead.
    fn nth_index(&self, n: usize) -> Option<usize> {
        self.toks[self.pos..]
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.kind.is_trivia())
            .nth(n)
            .map(|(i, _)| self.pos + i)
    }

    fn nth(&self, n: usize) -> SyntaxKind {
        self.nth_index(n).map_or(EOF, |i| self.toks[i].kind)
    }

    fn nth_text(&self, n: usize) -> &'a str {
        self.nth_index(n)
            .map_or("", |i| &self.src[self.toks[i].start..self.toks[i].end])
    }

    /// The `n`th token ahead starts exactly where the one before it ends.
    fn glued(&self, n: usize) -> bool {
        match (self.nth_index(n.wrapping_sub(1)), self.nth_index(n)) {
            (Some(a), Some(b)) if n > 0 => self.toks[a].end == self.toks[b].start,
            _ => false,
        }
    }

    /// No trivia between the last token bumped and the next one.
    fn glued_next(&self) -> bool {
        self.pos > 0 && self.nth_index(0) == Some(self.pos)
    }

    fn at(&self, k: SyntaxKind) -> bool {
        self.nth(0) == k
    }

    fn at_contextual(&self, word: &str) -> bool {
        self.at(IDENT) && self.nth_text(0) == word
    }

    /// A newline sits between the last significant token and the next one.
    fn on_new_line(&self) -> bool {
        let Some(i) = self.nth_index(0) else {
            return false;
        };
        self.toks[..i]
            .iter()
            .rev()
            .take_while(|t| t.kind.is_trivia())
            .any(|t| self.src[t.start..t.end].contains('\n'))
    }

    /// The next token is on a new line where a newline ends the construct.
    fn nl_stop(&self) -> bool {
        self.nl.last().copied().unwrap_or(true) && self.on_new_line()
    }

    /// The next token starts its line in column 0.
    fn at_col0(&self) -> bool {
        let Some(i) = self.nth_index(0) else {
            return false;
        };
        let start = self.toks[i].start;
        start == 0 || self.src.as_bytes()[start - 1] == b'\n'
    }

    // --- tree building ----------------------------------------------------

    fn flush_trivia(&mut self) {
        while let Some(t) = self.toks.get(self.pos) {
            if !t.kind.is_trivia() {
                break;
            }
            self.builder.token(t.kind.into(), &self.src[t.start..t.end]);
            self.pos += 1;
        }
    }

    fn bump(&mut self) {
        self.flush_trivia();
        if let Some(t) = self.toks.get(self.pos) {
            self.builder.token(t.kind.into(), &self.src[t.start..t.end]);
            self.pos += 1;
        }
    }

    fn start(&mut self, kind: SyntaxKind) {
        self.flush_trivia();
        self.builder.start_node(kind.into());
        self.depth += 1;
    }

    /// The root holds leading trivia too.
    fn start_root(&mut self) {
        self.builder.start_node(SOURCE_FILE.into());
        self.depth += 1;
    }

    fn checkpoint(&mut self) -> Checkpoint {
        self.flush_trivia();
        self.builder.checkpoint()
    }

    fn start_at(&mut self, cp: Checkpoint, kind: SyntaxKind) {
        self.builder.start_node_at(cp, kind.into());
        self.depth += 1;
    }

    fn finish(&mut self) {
        self.builder.finish_node();
        self.depth -= 1;
    }

    fn close_to(&mut self, depth: usize) {
        while self.depth > depth {
            self.finish();
        }
    }

    fn finish_all(&mut self) {
        // Trailing trivia belongs to the file.
        self.flush_trivia();
        self.close_to(0);
    }

    /// Run `f` with newlines significant (`true`) or not.
    fn with_nl<T>(&mut self, nl: bool, f: impl FnOnce(&mut Self) -> P<T>) -> P<T> {
        self.nl.push(nl);
        let r = f(self);
        self.nl.pop();
        r
    }

    // --- errors -----------------------------------------------------------

    fn found(&self) -> String {
        match self.nth(0) {
            EOF => "the end of the file".to_string(),
            ERROR_TOKEN => format!("unknown character `{}`", self.nth_text(0)),
            k if k.is_keyword() => format!("keyword `{}`", self.nth_text(0)),
            IDENT => format!("`{}`", self.nth_text(0)),
            PATH | STRING | INT | RANK => {
                format!("{} `{}`", self.nth(0).describe(), self.nth_text(0))
            }
            k => k.describe().to_string(),
        }
    }

    fn error_here(&mut self, message: String, hint: Option<String>) {
        let (start, end) = match self.nth_index(0) {
            Some(i) => (self.toks[i].start, self.toks[i].end),
            None => (self.src.len(), self.src.len()),
        };
        self.errors.push(ParseError {
            start,
            end,
            message,
            hint,
        });
    }

    fn err_expected<T>(&mut self, what: &str) -> P<T> {
        let msg = format!("expected {what}, found {}", self.found());
        let hint = self.hint();
        self.error_here(msg, hint);
        Err(Bail)
    }

    /// A hint for an error at the next token, when it looks like a slip.
    fn hint(&self) -> Option<String> {
        match self.nth(0) {
            NECK => return Some("a rule is `head if body`: `:-` is spelled `if`".to_string()),
            DOT if !self.glued_next() || matches!(self.nth(1), EOF) || self.on_new_line_at(1) => {
                return Some(
                    "a statement ends at the end of its line; there is no `.` terminator"
                        .to_string(),
                );
            }
            _ => {}
        }
        self.hyphen_hint()
    }

    /// The `n`th token ahead starts a new line.
    fn on_new_line_at(&self, n: usize) -> bool {
        match (self.nth_index(n.wrapping_sub(1)), self.nth_index(n)) {
            (Some(a), Some(b)) if n > 0 => self.toks[a + 1..b]
                .iter()
                .any(|t| self.src[t.start..t.end].contains('\n')),
            _ => false,
        }
    }

    /// The error sits inside a run of names, numbers and `-` with no
    /// spaces (`us-test-1a`): that was meant as one string.
    fn hyphen_hint(&self) -> Option<String> {
        let i = self.nth_index(0)?;
        let word = |k: SyntaxKind| matches!(k, IDENT | INT | MINUS) || k.is_keyword();
        let mut start = i;
        while start > 0
            && word(self.toks[start - 1].kind)
            && self.toks[start - 1].end == self.toks[start].start
        {
            start -= 1;
        }
        let mut end = i;
        while end < self.toks.len()
            && word(self.toks[end].kind)
            && (end == start || self.toks[end - 1].end == self.toks[end].start)
        {
            end += 1;
        }
        let run = &self.toks[start..end];
        if run.len() < 3 || !run.iter().any(|t| t.kind == MINUS) {
            return None;
        }
        let text = &self.src[run[0].start..run[run.len() - 1].end];
        Some(format!(
            "`-` is always an operator; a hyphenated name is a string: \"{text}\""
        ))
    }

    fn expect(&mut self, k: SyntaxKind) -> P {
        if self.at(k) {
            self.bump();
            Ok(())
        } else {
            self.err_expected(k.describe())
        }
    }

    fn eat(&mut self, k: SyntaxKind) -> bool {
        if self.at(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self) -> P {
        if word(self.nth(0)) {
            self.bump();
            Ok(())
        } else {
            self.err_expected("a name")
        }
    }

    /// `name (.name)*` with no spaces: a type, a stack, an extern.
    fn dotted(&mut self, what: &str) -> P {
        if !word(self.nth(0)) {
            return self.err_expected(what);
        }
        self.bump();
        while self.at(DOT) && self.glued_next() && word(self.nth(1)) && self.glued(1) {
            self.bump();
            self.bump();
        }
        Ok(())
    }

    /// Skip to the end of the abandoned statement that started at token
    /// `start`: up to the next line outside the brackets it opened, up to
    /// the `}` that closes the enclosing block, or up to a line that starts
    /// a statement in column 0.
    fn recover(&mut self, start: usize, in_block: bool) {
        let mut nest: i64 = 0;
        for t in &self.toks[start..self.pos] {
            match t.kind {
                L_PAREN | L_BRACE | L_BRACKET => nest += 1,
                R_PAREN | R_BRACE | R_BRACKET => nest -= 1,
                _ => {}
            }
        }
        let mut nest = nest.max(0);
        self.start(ERROR);
        let mut skipped = false;
        loop {
            match self.nth(0) {
                EOF => break,
                R_BRACE if nest == 0 && in_block => break,
                _ if skipped && nest == 0 && self.on_new_line() => break,
                k if skipped && self.on_new_line() && self.at_col0() && stmt_keyword(k) => break,
                L_PAREN | L_BRACE | L_BRACKET => nest += 1,
                R_PAREN | R_BRACE | R_BRACKET => nest = (nest - 1).max(0),
                _ => {}
            }
            self.bump();
            skipped = true;
        }
        self.finish();
    }

    // --- statements -------------------------------------------------------

    /// Statements, one per line, up to the end of the file or of a
    /// `{ ... }` block.
    fn stmts(&mut self, in_block: bool) {
        loop {
            match self.nth(0) {
                EOF => break,
                R_BRACE if in_block => break,
                _ => {}
            }
            let depth = self.depth;
            let nl = self.nl.len();
            self.flush_trivia();
            let start = self.pos;
            let ok = self.stmt().and_then(|()| self.stmt_end(in_block));
            if ok.is_err() {
                self.close_to(depth);
                self.nl.truncate(nl);
                self.recover(start, in_block);
            }
        }
    }

    /// A statement ends at a newline, the `}` of its block, or the file's end.
    fn stmt_end(&mut self, in_block: bool) -> P {
        match self.nth(0) {
            EOF => Ok(()),
            R_BRACE if in_block => Ok(()),
            _ if self.on_new_line() => Ok(()),
            _ => self.err_expected("the end of the line"),
        }
    }

    fn stmt(&mut self) -> P {
        let k = self.nth(0);
        let paren = self.nth(1) == L_PAREN && self.glued(1);
        match k {
            EDITION_KW => self.simple(EDITION, |p| p.expect(INT)),
            IMPORT_KW => self.simple(IMPORT, |p| {
                p.expect(STRING)?;
                if p.at_contextual("as") {
                    p.bump();
                    p.expect_word()?;
                }
                Ok(())
            }),
            PROVIDER_KW if !paren => self.simple(PROVIDER, |p| {
                p.expect_word()?;
                p.block()
            }),
            STACK_KW if !paren => self.simple(STACK, |p| {
                p.dotted("a stack name")?;
                // `stack app[env, region]`: the inputs that key it.
                if p.at(L_BRACKET) && p.glued_next() {
                    p.bump();
                    p.expect_word()?;
                    while p.eat(COMMA) {
                        p.expect_word()?;
                    }
                    p.expect(R_BRACKET)?;
                }
                p.block()
            }),
            INPUT_KW if !paren && self.nth_text(1) == "relation" && self.nth(2) != COLON => self
                .simple(INPUT_RELATION, |p| {
                    p.bump();
                    p.expect_word()?;
                    p.expect(SLASH)?;
                    p.expect(INT)?;
                    if !p.at_contextual("from") {
                        return p.err_expected("`from`");
                    }
                    p.bump();
                    p.term().map(drop)
                }),
            INPUT_KW if !paren => self.simple(INPUT, |p| {
                p.expect_word()?;
                p.expect(COLON)?;
                p.type_expr()?;
                if p.eat(EQ) {
                    p.term()?;
                }
                p.where_clause(false)
            }),
            OUTPUT_KW if !paren => self.simple(OUTPUT_DECL, |p| {
                p.expect_word()?;
                if p.eat(COLON) {
                    p.type_expr()
                } else {
                    p.expect(EQ)?;
                    p.term().map(drop)
                }
            }),
            EXPORT_KW => self.simple(EXPORT, |p| {
                p.expect_word()?;
                p.expect(SLASH)?;
                p.expect(INT)
            }),
            CONTRIBUTES_KW => self.simple(CONTRIBUTES, |p| {
                if !term_name(p.nth(0)) {
                    return p.err_expected(
                        "a grant: a relation, or TYPE.path, `_.path`, `settings.path`",
                    );
                }
                p.chain().map(drop)
            }),
            EXTERN_KW => self.simple(EXTERN, |p| {
                p.dotted("an extern name")?;
                if p.at(SLASH) {
                    let msg = format!("expected `(`, found {}", p.found());
                    p.error_here(msg, Some("`extern p/N` is spelled `decl p/N`".to_string()));
                    return Err(Bail);
                }
                p.expect(L_PAREN)?;
                p.with_nl(false, |p| {
                    loop {
                        p.bind_arg()?;
                        if !p.eat(COMMA) {
                            break;
                        }
                    }
                    p.expect(R_PAREN)
                })?;
                p.eat(PERSIST_KW);
                Ok(())
            }),
            TYPE_KW if !paren => self.simple(TYPE_DECL, |p| {
                p.dotted("a type name")?;
                p.attr_block()
            }),
            DECL_KW => self.simple(DECL, |p| p.decl()),
            MODULE_KW | POLICY_KW | SCENARIO_KW => {
                let kind = match k {
                    MODULE_KW => MODULE,
                    POLICY_KW => POLICY,
                    _ => SCENARIO,
                };
                self.simple(kind, |p| {
                    p.expect_word()?;
                    p.stmt_block()
                })
            }
            APPLY_KW => self.simple(APPLY, |p| p.expect_word()),
            INSTANCE_KW => self.simple(INSTANCE, |p| {
                p.expect_word()?;
                p.expect_word()?;
                p.block()
            }),
            WHEN_KW | FOR_KW => {
                let kind = if k == WHEN_KW { WHEN } else { FOR_STMT };
                self.simple(kind, |p| {
                    p.body1()?;
                    p.stmt_block()
                })
            }
            LET_KW => self.simple(LET, |p| {
                p.expect_word()?;
                p.expect(EQ)?;
                p.term().map(drop)
            }),
            WITH_KW => self.simple(WITH, |p| {
                p.expect_word()?;
                p.expect(EQ)?;
                p.term().map(drop)
            }),
            RESOURCE_KW => self.simple(RESOURCE, |p| {
                if !word(p.nth(0)) {
                    return p.err_expected("a resource type");
                }
                p.dotted("a resource type")?;
                if word(p.nth(0)) || p.at(STRING) {
                    p.bump();
                } else {
                    return p.err_expected("a resource name (a name or a string)");
                }
                p.eat(RANK);
                p.block()
            }),
            SETTINGS_KW if !(paren || matches!(self.nth(1), DOT | L_BRACKET) && self.glued(1)) => {
                self.simple(SETTINGS, |p| {
                    if word(p.nth(0)) || p.at(STRING) {
                        p.bump();
                    } else {
                        return p.err_expected("an environment (a name or a string)");
                    }
                    p.eat(RANK);
                    p.block()
                })
            }
            DENY_KW | WARN_KW | CONSTRAINT_KW if self.nth(1) == STRING => self.simple(CHECK, |p| {
                p.bump();
                if p.at(L_BRACE) && !p.on_new_line() {
                    p.with_nl(false, |p| p.object().map(drop))?;
                }
                p.opt_if_body()
            }),
            _ if term_name(k) && matches!(self.nth(1), EQ | PLUS_EQ) => {
                self.simple(VALUE_RULE, |p| {
                    p.bump();
                    p.term()?;
                    p.eat(RANK);
                    p.opt_if_body()
                })
            }
            _ if term_name(k)
                && old_spelling(self.nth_text(0)).is_some()
                && !matches!(self.nth(1), L_PAREN | EQ | DOT) =>
            {
                let msg = format!("expected a statement, found {}", self.found());
                let hint = old_spelling(self.nth_text(0)).map(str::to_string);
                self.error_here(msg, hint);
                Err(Bail)
            }
            _ if term_name(k) => {
                let cp = self.checkpoint();
                let kind = self.chain_term()?;
                if matches!(self.nth(0), EQ | PLUS_EQ) && !self.on_new_line() {
                    self.start_at(cp, CONTRIBUTION);
                    self.bump();
                    self.term()?;
                    self.eat(RANK);
                    self.opt_if_body()?;
                    self.finish();
                    return Ok(());
                }
                if !matches!(kind, CALL | RECORD_ATOM) {
                    let msg = format!("expected `(` or `=` after the name, found {}", self.found());
                    let hint = self.hint();
                    self.error_here(msg, hint);
                    return Err(Bail);
                }
                self.eat(RANK);
                let has_body = self.at(IF_KW) && !self.on_new_line();
                self.start_at(cp, if has_body { RULE } else { FACT });
                self.opt_if_body()?;
                self.finish();
                Ok(())
            }
            _ => {
                let msg = format!("expected a statement, found {}", self.found());
                let hint = old_spelling(self.nth_text(0))
                    .map(str::to_string)
                    .or_else(|| {
                        (k == IF_KW)
                            .then(|| "`if` goes on the line of the head it guards".to_string())
                    })
                    .or_else(|| self.hint());
                self.error_here(msg, hint);
                Err(Bail)
            }
        }
    }

    /// A statement node of `kind`: its keyword, then `rest`.
    fn simple(&mut self, kind: SyntaxKind, rest: impl FnOnce(&mut Self) -> P) -> P {
        self.start(kind);
        self.bump();
        rest(self)?;
        self.finish();
        Ok(())
    }

    /// `[if body]` on the line the statement is on.
    fn opt_if_body(&mut self) -> P {
        if self.at(NECK) && !self.on_new_line() {
            return self.err_expected("`if` or the end of the line");
        }
        if self.at(IF_KW) && !self.on_new_line() {
            self.bump();
            self.body()?;
        }
        Ok(())
    }

    /// `decl p/N [mixed]`, `decl p(field: type, ...)`, `decl type Q open`.
    fn decl(&mut self) -> P {
        if self.at(TYPE_KW) {
            self.bump();
            self.dotted("a type name")?;
            if !self.at_contextual("open") {
                return self.err_expected("`open`");
            }
            self.bump();
            return Ok(());
        }
        self.dotted("a relation name")?;
        if self.eat(SLASH) {
            self.expect(INT)?;
            if self.at_contextual("mixed") {
                self.bump();
            }
            return Ok(());
        }
        if !self.at(L_PAREN) {
            return self.err_expected("`/` or `(`");
        }
        self.bump();
        self.with_nl(false, |p| {
            loop {
                p.start(BIND_ARG);
                p.expect_word()?;
                p.expect(COLON)?;
                p.type_expr()?;
                p.finish();
                if !p.eat(COMMA) || p.at(R_PAREN) {
                    break;
                }
            }
            p.expect(R_PAREN)
        })
    }

    fn bind_arg(&mut self) -> P {
        self.start(BIND_ARG);
        if !(self.eat(PLUS) || self.eat(MINUS)) {
            return self.err_expected("`+` (input) or `-` (output)");
        }
        self.expect_word()?;
        if self.eat(COLON) {
            self.type_expr()?;
        }
        self.finish();
        Ok(())
    }

    /// `{ stmt* }`, one per line.
    fn stmt_block(&mut self) -> P {
        self.start(STMT_BLOCK);
        self.expect(L_BRACE)?;
        self.nl.push(true);
        self.stmts(true);
        self.nl.pop();
        self.expect(R_BRACE)?;
        self.finish();
        Ok(())
    }

    /// `{ clause* assign* }` of a resource, settings, instance, provider or
    /// stack: entries separated by a newline or a comma.
    fn block(&mut self) -> P {
        self.start(BLOCK);
        self.expect(L_BRACE)?;
        self.with_nl(true, |p| {
            while !p.at(R_BRACE) {
                if matches!(p.nth(0), FOR_KW | IF_KW) {
                    p.start(CLAUSE);
                    p.bump();
                    p.body1()?;
                    p.finish();
                } else {
                    p.assign()?;
                }
                if !p.eat(COMMA) && !p.at(R_BRACE) && !p.on_new_line() {
                    return p.err_expected("`,`, a new line or `}`");
                }
            }
            Ok(())
        })?;
        self.bump();
        self.finish();
        Ok(())
    }

    fn assign(&mut self) -> P {
        self.start(ASSIGN);
        self.block_path()?;
        if !(self.eat(EQ) || self.eat(PLUS_EQ)) {
            return self.err_expected("`=` or `+=`");
        }
        self.term()?;
        self.eat(RANK);
        self.finish();
        Ok(())
    }

    /// A keypath without its leading dot: `a.b[0]."c-d"`.
    fn block_path(&mut self) -> P {
        self.start(BLOCK_PATH);
        if word(self.nth(0)) || self.at(STRING) {
            self.bump();
        } else {
            return self.err_expected("an attribute path");
        }
        loop {
            if self.at(DOT) && self.glued_next() {
                self.bump();
                if word(self.nth(0)) || self.at(STRING) {
                    self.bump();
                } else {
                    return self.err_expected("a path segment");
                }
            } else if self.at(L_BRACKET) && self.glued_next() {
                self.bump();
                self.expect(INT)?;
                self.expect(R_BRACKET)?;
            } else {
                break;
            }
        }
        self.finish();
        Ok(())
    }

    /// `{ attrdecl* }` of a user type: `path: type flag* [where body]` or a
    /// nested `path: { ... }`, separated by a newline or a comma.
    fn attr_block(&mut self) -> P {
        self.expect(L_BRACE)?;
        self.with_nl(true, |p| {
            while !p.at(R_BRACE) {
                p.start(ATTR_DECL);
                p.block_path()?;
                p.expect(COLON)?;
                if p.at(L_BRACE) {
                    p.attr_block()?;
                } else {
                    p.type_expr()?;
                    while p.at(IDENT)
                        && matches!(
                            p.nth_text(0),
                            "required" | "computed" | "id" | "sensitive" | "nullable"
                        )
                        && !p.at_attr_decl()
                        && !p.on_new_line()
                    {
                        p.bump();
                    }
                    p.where_clause(true)?;
                }
                p.finish();
                if !p.eat(COMMA) && !p.at(R_BRACE) && !p.on_new_line() {
                    return p.err_expected("`,`, a new line or `}`");
                }
            }
            Ok(())
        })?;
        self.bump();
        Ok(())
    }

    /// The next tokens start an attribute declaration: `path:`.
    fn at_attr_decl(&self) -> bool {
        (word(self.nth(0)) || self.nth(0) == STRING) && self.nth(1) == COLON
    }

    fn where_clause(&mut self, in_type: bool) -> P {
        if !self.at(WHERE_KW) || self.on_new_line() {
            return Ok(());
        }
        self.start(WHERE_CLAUSE);
        self.bump();
        self.start(BODY);
        loop {
            self.lit()?;
            if !self.at(COMMA) || (in_type && self.attr_decl_after_comma()) {
                break;
            }
            self.bump();
        }
        self.finish();
        self.finish();
        Ok(())
    }

    fn attr_decl_after_comma(&self) -> bool {
        (word(self.nth(1)) || self.nth(1) == STRING) && self.nth(2) == COLON
    }

    /// `type := name | name(type, ...) | { name: type, ... } | STRING`
    fn type_expr(&mut self) -> P {
        self.start(TYPE_EXPR);
        match self.nth(0) {
            STRING => self.bump(),
            L_BRACE => {
                self.bump();
                self.with_nl(false, |p| {
                    while !p.at(R_BRACE) {
                        p.start(OBJECT_FIELD);
                        if word(p.nth(0)) || p.at(STRING) {
                            p.bump();
                        } else {
                            return p.err_expected("a field name");
                        }
                        p.expect(COLON)?;
                        p.type_expr()?;
                        p.finish();
                        if !p.eat(COMMA) {
                            break;
                        }
                    }
                    p.expect(R_BRACE)
                })?;
            }
            k if word(k) => {
                self.dotted("a type")?;
                if self.at(L_PAREN) && self.glued_next() {
                    self.bump();
                    self.with_nl(false, |p| {
                        loop {
                            p.type_expr()?;
                            if !p.eat(COMMA) {
                                break;
                            }
                        }
                        p.expect(R_PAREN)
                    })?;
                }
            }
            _ => return self.err_expected("a type"),
        }
        self.finish();
        Ok(())
    }

    // --- bodies and literals ---------------------------------------------

    /// `{ lit (, | newline) lit ... }` or `lit, lit, ...`.
    fn body(&mut self) -> P {
        if self.at(L_BRACE) {
            return self.body_block();
        }
        self.body1()
    }

    fn body1(&mut self) -> P {
        self.start(BODY);
        loop {
            self.lit()?;
            if !self.eat(COMMA) {
                break;
            }
        }
        self.finish();
        Ok(())
    }

    fn body_block(&mut self) -> P {
        self.start(BODY);
        self.bump();
        self.with_nl(true, |p| {
            while !p.at(R_BRACE) {
                p.lit()?;
                if !p.eat(COMMA) && !p.at(R_BRACE) && !p.on_new_line() {
                    return p.err_expected("`,`, a new line or `}`");
                }
            }
            Ok(())
        })?;
        self.bump();
        self.finish();
        Ok(())
    }

    fn lit(&mut self) -> P {
        if self.at(NOT_KW) && self.nth(1) == L_BRACE {
            self.start(LIT_NOT_BLOCK);
            self.bump();
            self.body_block()?;
            self.finish();
            return Ok(());
        }
        if self.at(NOT_KW) && self.nth(1) != IN_KW {
            self.start(LIT_NOT);
            self.bump();
            self.lit1()?;
            self.finish();
            return Ok(());
        }
        self.lit1()
    }

    fn lit1(&mut self) -> P {
        match self.nth(0) {
            EXISTS_KW => {
                self.start(LIT_EXISTS);
                self.bump();
                self.term()?;
                self.finish();
                return Ok(());
            }
            HAS_KW => {
                self.start(LIT_HAS);
                self.bump();
                self.term()?;
                self.finish();
                return Ok(());
            }
            SOME_KW => {
                self.start(LIT_SOME);
                self.bump();
                self.term()?;
                if self.eat(COMMA) {
                    self.term()?;
                }
                self.expect(IN_KW)?;
                self.term()?;
                self.finish();
                return Ok(());
            }
            _ => {}
        }
        let cp = self.checkpoint();
        let kind = self.term()?;
        let next = if self.nl_stop() { EOF } else { self.nth(0) };
        match next {
            k if is_cmp(k) => {
                self.start_at(cp, LIT_CMP);
                // `lo <= x <= hi` chains: each operator compares its neighbours.
                while is_cmp(self.nth(0)) && !self.nl_stop() {
                    self.bump();
                    self.term()?;
                }
            }
            IN_KW => {
                self.start_at(cp, LIT_IN);
                self.bump();
                if self.at(RESOURCE_KW) {
                    self.bump();
                } else {
                    self.term()?;
                }
            }
            NOT_KW if self.nth(1) == IN_KW => {
                self.start_at(cp, LIT_NOT_IN);
                self.bump();
                self.bump();
                self.term()?;
            }
            _ if matches!(kind, CALL | RECORD_ATOM) => self.start_at(cp, LIT_ATOM),
            _ if kind == CHAIN => self.start_at(cp, LIT_TRUTH),
            _ => return self.err_expected("a comparison, `in` or `not in` after the term"),
        }
        self.finish();
        Ok(())
    }

    fn arg_list(&mut self) -> P {
        self.start(ARG_LIST);
        self.expect(L_PAREN)?;
        self.with_nl(false, |p| {
            while !p.at(R_PAREN) {
                p.term()?;
                if !p.eat(COMMA) {
                    break;
                }
            }
            p.expect(R_PAREN)
        })?;
        self.finish();
        Ok(())
    }

    // --- terms ------------------------------------------------------------

    /// A term; returns the kind of its outermost node.
    fn term(&mut self) -> P<SyntaxKind> {
        self.expr(0)
    }

    fn expr(&mut self, min_bp: u8) -> P<SyntaxKind> {
        let cp = self.checkpoint();
        let mut kind = if self.at(MINUS) {
            self.start(UNARY_EXPR);
            self.bump();
            self.expr(5)?;
            self.finish();
            UNARY_EXPR
        } else {
            self.primary()?
        };
        loop {
            if self.nl_stop() {
                break;
            }
            let (l, r) = match self.nth(0) {
                PLUS | MINUS => (1, 2),
                STAR | SLASH | PERCENT => (3, 4),
                _ => break,
            };
            if l < min_bp {
                break;
            }
            self.start_at(cp, BIN_EXPR);
            self.bump();
            self.expr(r)?;
            self.finish();
            kind = BIN_EXPR;
        }
        Ok(kind)
    }

    fn primary(&mut self) -> P<SyntaxKind> {
        let k = self.nth(0);
        match k {
            INT | STRING | TRUE_KW | FALSE_KW | NULL_KW => self.leaf(LITERAL),
            PATH => self.leaf(PATH_LIT),
            L_PAREN => {
                self.start(PAREN);
                self.bump();
                self.with_nl(false, |p| {
                    p.term()?;
                    p.expect(R_PAREN)
                })?;
                self.finish();
                Ok(PAREN)
            }
            L_BRACKET => self.list(),
            L_BRACE => self.object(),
            _ if term_name(k) => self.chain_term(),
            _ => self.err_expected("a term"),
        }
    }

    fn leaf(&mut self, kind: SyntaxKind) -> P<SyntaxKind> {
        self.start(kind);
        self.bump();
        self.finish();
        Ok(kind)
    }

    /// A chain, then a glued `(` makes it a call and a glued `{` after a
    /// plain name a record atom.
    fn chain_term(&mut self) -> P<SyntaxKind> {
        let cp = self.checkpoint();
        let plain = self.chain()?;
        if self.at(L_PAREN) && self.glued_next() {
            self.start_at(cp, CALL);
            self.arg_list()?;
            self.finish();
            return Ok(CALL);
        }
        if plain && self.at(L_BRACE) && self.glued_next() {
            self.start_at(cp, RECORD_ATOM);
            self.bump();
            self.with_nl(false, |p| {
                while !p.at(R_BRACE) {
                    p.start(RECORD_FIELD);
                    p.expect_word()?;
                    p.expect(COLON)?;
                    p.term()?;
                    p.finish();
                    if !p.eat(COMMA) {
                        break;
                    }
                }
                p.expect(R_BRACE)
            })?;
            self.finish();
            return Ok(RECORD_ATOM);
        }
        Ok(CHAIN)
    }

    /// `name (.seg | [t, ...] | /name)*`, each part glued to the last.
    /// Returns whether the chain is a single name.
    fn chain(&mut self) -> P<bool> {
        self.start(CHAIN);
        self.bump();
        let mut plain = true;
        loop {
            if self.at(DOT) && self.glued_next() {
                self.bump();
                if word(self.nth(0)) || self.at(STRING) {
                    self.bump();
                } else {
                    return self.err_expected("a name after `.`");
                }
            } else if self.at(L_BRACKET) && self.glued_next() {
                self.start(INDEX);
                self.bump();
                self.with_nl(false, |p| {
                    loop {
                        p.term()?;
                        if !p.eat(COMMA) {
                            break;
                        }
                    }
                    p.expect(R_BRACKET)
                })?;
                self.finish();
            } else if self.at(SLASH) && self.glued_next() && word(self.nth(1)) && self.glued(1) {
                self.bump();
                self.bump();
            } else {
                break;
            }
            plain = false;
        }
        self.finish();
        Ok(plain)
    }

    /// `[a, b]`, `[item | body]`, `[item ordered by key | body]`.
    fn list(&mut self) -> P<SyntaxKind> {
        let cp = self.checkpoint();
        self.bump();
        self.with_nl(false, |p| {
            if p.at(R_BRACKET) {
                p.start_at(cp, LIST);
                p.bump();
                p.finish();
                return Ok(LIST);
            }
            p.term()?;
            if p.at(PIPE) || p.at_contextual("ordered") {
                p.start_at(cp, COMPREHENSION);
                if p.at_contextual("ordered") {
                    p.bump();
                    if !p.at_contextual("by") {
                        return p.err_expected("`by`");
                    }
                    p.bump();
                    p.term()?;
                }
                p.expect(PIPE)?;
                p.body1()?;
                p.expect(R_BRACKET)?;
                p.finish();
                return Ok(COMPREHENSION);
            }
            p.start_at(cp, LIST);
            while p.eat(COMMA) {
                if p.at(R_BRACKET) {
                    break;
                }
                p.term()?;
            }
            p.expect(R_BRACKET)?;
            p.finish();
            Ok(LIST)
        })
    }

    /// `{ key: term, name, ... }`: a name alone is `name: name`.
    fn object(&mut self) -> P<SyntaxKind> {
        self.start(OBJECT);
        self.bump();
        self.with_nl(false, |p| {
            while !p.at(R_BRACE) {
                p.start(OBJECT_FIELD);
                if word(p.nth(0)) || p.at(STRING) {
                    p.bump();
                } else {
                    return p.err_expected("an object key (a name or a string)");
                }
                if p.eat(COLON) {
                    p.term()?;
                }
                p.finish();
                if !p.eat(COMMA) {
                    break;
                }
            }
            p.expect(R_BRACE)
        })?;
        self.finish();
        Ok(OBJECT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(src: &str) -> Vec<String> {
        parse(src).errors.into_iter().map(|e| e.message).collect()
    }

    fn kinds(src: &str, want: &[SyntaxKind]) -> Vec<SyntaxKind> {
        parse(src)
            .syntax()
            .descendants()
            .map(|n| n.kind())
            .filter(|k| want.contains(k))
            .collect()
    }

    #[test]
    fn lossless_even_with_errors() {
        for src in [
            "edition 2026\np(a) if q(x), x > 1 # c\n",
            "p(a if\nq(b)\n}}\n",
            "resource net.vpc main { cidr = \"x\" }\nq(b)",
        ] {
            assert_eq!(parse(src).syntax().to_string(), src);
        }
    }

    #[test]
    fn three_errors_three_diagnostics() {
        let src = "p(a) if q(]\nok(1)\nr(b) if ,\nok(2)\nresource x { }\nok(3)\n";
        assert_eq!(errors(src).len(), 3, "{:?}", errors(src));
    }

    #[test]
    fn a_newline_ends_a_statement() {
        assert!(errors("p(a)\nq(b)\nr(c) if q(c),\n  p(c)\n").is_empty());
        let e = parse("p(a) q(b)\n").errors;
        assert_eq!(e.len(), 1);
        assert!(
            e[0].message.starts_with("expected the end of the line"),
            "{e:?}"
        );
    }

    #[test]
    fn an_old_terminator_says_so() {
        let e = parse("p(a).\nq(b).\n").errors;
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].hint.as_deref().unwrap().contains("no `.` terminator"));
        let e = parse("p(a) :- q(a)\n").errors;
        assert!(
            e[0].hint.as_deref().unwrap().contains("spelled `if`"),
            "{e:?}"
        );
    }

    #[test]
    fn statement_shapes() {
        let src = "k = 1 if p(1)\nr.tags = {} if r in resource\ndeny \"m\" { a } if p(a)\n\
                   p(x) if { q(x)\n r(x) }\nlet cfg = settings[env]\n";
        assert_eq!(
            kinds(src, &[VALUE_RULE, CONTRIBUTION, CHECK, RULE, LET]),
            vec![VALUE_RULE, CONTRIBUTION, CHECK, RULE, LET]
        );
    }

    #[test]
    fn literal_shapes() {
        let src = "p(x) if q(x), x.a, x.b == 1, x in net.vpc, has x.c, exists y, \
                   not x.d, some i, v in xs, x not in ys, not { r(x) }, e{a: x}\n";
        assert_eq!(
            kinds(
                src,
                &[
                    LIT_ATOM,
                    LIT_TRUTH,
                    LIT_CMP,
                    LIT_IN,
                    LIT_HAS,
                    LIT_EXISTS,
                    LIT_NOT,
                    LIT_SOME,
                    LIT_NOT_IN,
                    LIT_NOT_BLOCK
                ]
            ),
            vec![
                LIT_ATOM,
                LIT_TRUTH,
                LIT_CMP,
                LIT_IN,
                LIT_HAS,
                LIT_EXISTS,
                LIT_NOT,
                LIT_TRUTH,
                LIT_SOME,
                LIT_NOT_IN,
                LIT_NOT_BLOCK,
                LIT_ATOM,
                LIT_ATOM
            ]
        );
    }

    #[test]
    fn blocks_take_clauses_then_fields() {
        let src = "resource net.subnet \"s-{z}\" {\n  for data(\"zone\", z)\n  if z != \"x\"\n  \
                   cidr = inet_subnet(vpc.cidr, 4, zone_index[z])\n  zone = z\n}\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(
            kinds(src, &[CLAUSE, ASSIGN]),
            vec![CLAUSE, CLAUSE, ASSIGN, ASSIGN]
        );
    }

    #[test]
    fn chains_are_glued() {
        let src = "p(m.i/n.x, a / b, t[e].p, f(x))\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(kinds(src, &[CHAIN]).len(), 8);
        assert_eq!(kinds(src, &[INDEX, BIN_EXPR]), vec![BIN_EXPR, INDEX]);
    }

    #[test]
    fn errors_inside_a_block_recover_at_the_brace() {
        let src = "module m {\n  p(a) if ,\n  q(b)\n}\nr(c)\n";
        assert_eq!(errors(src).len(), 1, "{:?}", errors(src));
    }
}
