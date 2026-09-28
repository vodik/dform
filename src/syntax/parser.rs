//! Recursive descent over the token stream into a lossless rowan tree
//! (docs/grammar.md). Statements are hand-written productions, terms a Pratt
//! parser. An error inside a statement abandons it: the rest of it, up to
//! the `.` that ends it or the `}` that ends its block, becomes one ERROR
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

/// The end of input; never a real token.
const EOF: SyntaxKind = SyntaxKind::__LAST;

/// An abandoned statement. The error is already recorded.
struct Bail;
type P<T = ()> = Result<T, Bail>;

/// Where a term may be an address `qname/ident` (E §6 disambiguation): the
/// second argument of these predicates and functions, and `moved`'s third.
fn addr_position(name: &str, index: usize) -> bool {
    match name {
        "want" | "arg" | "arg_add" | "attr" | "adopt" | "lifecycle" | "ignore_changes" | "ref" => {
            index == 1
        }
        "moved" => index == 1 || index == 2,
        _ => false,
    }
}

/// Before edition 2026 some statements had other spellings.
fn old_spelling(word: &str) -> Option<&'static str> {
    Some(match word {
        "component_def" => "`component_def` is spelled `module` in edition 2026",
        "use" => "`use` is spelled `instance` in edition 2026",
        "component" => "a component is a `module` and an `instance` of it in edition 2026",
        "policy_pack" => "`policy_pack` is spelled `policy` in edition 2026",
        "apply_policy" => "`apply_policy` is spelled `apply` in edition 2026",
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

    fn at_name(&self) -> bool {
        self.nth(0).is_name()
    }

    fn at_contextual(&self, word: &str) -> bool {
        self.at(IDENT) && self.nth_text(0) == word
    }

    /// The next token ends a line: a newline sits between it and the one
    /// before.
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

    // --- errors -----------------------------------------------------------

    fn found(&self) -> String {
        match self.nth(0) {
            EOF => "the end of the file".to_string(),
            ERROR_TOKEN => format!("unknown character `{}`", self.nth_text(0)),
            k if k.is_keyword() => format!("keyword `{}`", self.nth_text(0)),
            IDENT | VAR | QNAME | FIELD | PATH | STRING | INT | RANK => {
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
        let hint = self.hyphen_hint();
        self.error_here(msg, hint);
        Err(Bail)
    }

    /// The error sits inside a run of names, numbers and `-` with no
    /// spaces (`us-test-1a`): that was meant as one name.
    fn hyphen_hint(&self) -> Option<String> {
        let i = self.nth_index(0)?;
        let word = |k: SyntaxKind| matches!(k, IDENT | VAR | QNAME | INT | MINUS) || k.is_keyword();
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

    fn expect_name(&mut self) -> P {
        if self.at_name() {
            self.bump();
            Ok(())
        } else {
            self.err_expected("a name")
        }
    }

    /// Skip to the end of the abandoned statement: through its `.`, or up to
    /// the `}` that closes the enclosing block. Brackets opened while
    /// skipping are balanced first.
    fn recover(&mut self, in_block: bool) {
        self.start(ERROR);
        let mut nest = 0usize;
        loop {
            match self.nth(0) {
                EOF => break,
                DOT if nest == 0 => {
                    self.bump();
                    break;
                }
                R_BRACE if nest == 0 && in_block => break,
                L_PAREN | L_BRACE | L_BRACKET => nest += 1,
                R_PAREN | R_BRACE | R_BRACKET => nest = nest.saturating_sub(1),
                _ => {}
            }
            self.bump();
        }
        self.finish();
    }

    // --- statements -------------------------------------------------------

    /// `(stmt ".")*` up to the end of the file or of a `{ ... }` block.
    fn stmts(&mut self, in_block: bool) {
        loop {
            match self.nth(0) {
                EOF => break,
                R_BRACE if in_block => break,
                _ => {}
            }
            let depth = self.depth;
            if self.stmt().is_err() {
                self.close_to(depth);
                self.recover(in_block);
            }
        }
    }

    fn stmt(&mut self) -> P {
        let k = self.nth(0);
        let paren = self.nth(1) == L_PAREN;
        match k {
            EDITION_KW => self.simple(EDITION, |p| p.expect(INT))?,
            IMPORT_KW => self.simple(IMPORT, |p| {
                p.expect(STRING)?;
                if p.at_contextual("as") {
                    p.bump();
                    p.expect_name()?;
                }
                Ok(())
            })?,
            PROVIDER_KW if !paren => self.simple(PROVIDER, |p| {
                p.expect_name()?;
                p.block()
            })?,
            STACK_KW if !paren => self.simple(STACK, |p| {
                p.qname()?;
                p.block()
            })?,
            INPUT_KW if !paren => self.simple(INPUT, |p| {
                p.expect_name()?;
                p.expect(COLON)?;
                p.type_expr()?;
                if p.eat(EQ) {
                    p.term(false)?;
                }
                p.where_clause(false)
            })?,
            OUTPUT_KW if !paren => self.simple(OUTPUT_DECL, |p| {
                p.expect_name()?;
                if p.eat(COLON) {
                    p.type_expr()
                } else {
                    p.expect(EQ)?;
                    p.term(false).map(drop)
                }
            })?,
            EXPORT_KW => self.simple(EXPORT, |p| {
                p.expect_name()?;
                p.expect(SLASH)?;
                p.expect(INT)
            })?,
            CONTRIBUTES_KW => self.simple(CONTRIBUTES, |p| {
                if p.at_contextual("arg") && p.nth(1) == IDENT && p.nth_text(1) == "to" {
                    p.bump();
                    p.bump();
                    // typepat := QNAME | IDENT | "_"
                    if p.at(QNAME) || p.at_name() || (p.at(VAR) && p.nth_text(0) == "_") {
                        p.bump();
                    } else {
                        return p.err_expected("a type or `_`");
                    }
                    if !p.at_contextual("at") {
                        return p.err_expected("`at`");
                    }
                    p.bump();
                    if p.at(PATH) || (p.at(VAR) && p.nth_text(0) == "_") {
                        p.bump();
                        Ok(())
                    } else {
                        p.err_expected("a keypath or `_`")
                    }
                } else {
                    p.expect_name()
                }
            })?,
            EXTERN_KW => self.simple(EXTERN, |p| {
                p.qname()?;
                if p.at(SLASH) {
                    let msg = format!("expected `(`, found {}", p.found());
                    p.error_here(
                        msg,
                        Some("`extern p/N` is spelled `decl p/N` in edition 2026".to_string()),
                    );
                    return Err(Bail);
                }
                p.expect(L_PAREN)?;
                loop {
                    p.bind_arg()?;
                    if !p.eat(COMMA) {
                        break;
                    }
                }
                p.expect(R_PAREN)?;
                p.eat(PERSIST_KW);
                Ok(())
            })?,
            TYPE_KW if !paren => self.simple(TYPE_DECL, |p| {
                if p.at(QNAME) || p.at_name() {
                    p.bump();
                } else {
                    return p.err_expected("a type name");
                }
                p.attr_block()
            })?,
            DECL_KW => self.simple(DECL, |p| p.decl())?,
            MODULE_KW => self.simple(MODULE, |p| {
                p.expect_name()?;
                p.stmt_block()
            })?,
            POLICY_KW => self.simple(POLICY, |p| {
                p.expect_name()?;
                p.stmt_block()
            })?,
            SCENARIO_KW => self.simple(SCENARIO, |p| {
                p.expect_name()?;
                p.stmt_block()
            })?,
            APPLY_KW => self.simple(APPLY, |p| p.expect_name())?,
            INSTANCE_KW => self.simple(INSTANCE, |p| {
                p.expect_name()?;
                p.expect_name()?;
                p.block()?;
                p.opt_body()
            })?,
            WHEN_KW => self.simple(WHEN, |p| {
                p.lit()?;
                p.stmt_block()
            })?,
            RESOURCE_KW => self.simple(RESOURCE, |p| {
                if p.at(QNAME) || p.at_name() {
                    p.bump();
                } else {
                    return p.err_expected("a resource type");
                }
                if p.at_name() || p.at(VAR) {
                    p.bump();
                } else {
                    return p.err_expected("a resource name (an identifier or a variable)");
                }
                p.eat(RANK);
                p.block()?;
                p.opt_body()
            })?,
            SETTINGS_KW if !paren => self.simple(SETTINGS, |p| {
                if p.at_name() || p.at(VAR) {
                    p.bump();
                } else {
                    return p.err_expected("an environment (an identifier or a variable)");
                }
                p.eat(RANK);
                p.block()?;
                p.opt_body()
            })?,
            _ => {
                let cp = self.checkpoint();
                if (self.at_name() || self.at(QNAME)) && matches!(self.nth(1), L_PAREN | L_BRACE) {
                    self.atom()?;
                } else {
                    let msg = format!("expected a statement, found {}", self.found());
                    let hint = old_spelling(self.nth_text(0)).map(str::to_string);
                    self.error_here(msg, hint);
                    return Err(Bail);
                }
                self.eat(RANK);
                if self.at(NECK) {
                    self.start_at(cp, RULE);
                    self.bump();
                    self.body()?;
                } else {
                    self.start_at(cp, FACT);
                }
                self.finish();
            }
        }
        self.terminator()
    }

    /// A statement node of `kind`: its keyword, then `rest`.
    fn simple(&mut self, kind: SyntaxKind, rest: impl FnOnce(&mut Self) -> P) -> P {
        self.start(kind);
        self.bump();
        rest(self)?;
        self.finish();
        Ok(())
    }

    fn terminator(&mut self) -> P {
        if self.eat(DOT) {
            return Ok(());
        }
        let hint = self.on_new_line().then(|| {
            "every statement ends with `.`; is one missing on the line above?".to_string()
        });
        let msg = format!("expected `.` to end the statement, found {}", self.found());
        self.error_here(msg, hint);
        Err(Bail)
    }

    fn qname(&mut self) -> P {
        if self.at(QNAME) || self.at_name() {
            self.bump();
            Ok(())
        } else {
            self.err_expected("a qualified name")
        }
    }

    fn opt_body(&mut self) -> P {
        if self.eat(NECK) {
            self.body()?;
        }
        Ok(())
    }

    /// `decl p/N [mixed]`, `decl p(V: type, ...)`, `decl type Q open`.
    fn decl(&mut self) -> P {
        if self.at(TYPE_KW) {
            self.bump();
            self.qname()?;
            if !self.at_contextual("open") {
                return self.err_expected("`open`");
            }
            self.bump();
            return Ok(());
        }
        self.qname()?;
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
        loop {
            self.start(BIND_ARG);
            self.expect(VAR)?;
            self.expect(COLON)?;
            self.type_expr()?;
            self.finish();
            if !self.eat(COMMA) || self.at(R_PAREN) {
                break;
            }
        }
        self.expect(R_PAREN)
    }

    fn bind_arg(&mut self) -> P {
        self.start(BIND_ARG);
        if !(self.eat(PLUS) || self.eat(MINUS)) {
            return self.err_expected("`+` (input) or `-` (output)");
        }
        self.expect_name()?;
        if self.eat(COLON) {
            self.type_expr()?;
        }
        self.finish();
        Ok(())
    }

    /// `{ (stmt ".")* }`
    fn stmt_block(&mut self) -> P {
        self.start(STMT_BLOCK);
        self.expect(L_BRACE)?;
        self.stmts(true);
        self.expect(R_BRACE)?;
        self.finish();
        Ok(())
    }

    /// `{ assign* }`, assignments separated by optional commas.
    fn block(&mut self) -> P {
        self.start(BLOCK);
        self.expect(L_BRACE)?;
        while !self.at(R_BRACE) {
            self.assign()?;
            if !self.eat(COMMA) && !self.at(R_BRACE) && !self.on_new_line() {
                return self.err_expected("`,`, a new line or `}`");
            }
        }
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
        self.term(false)?;
        self.eat(RANK);
        self.finish();
        Ok(())
    }

    /// A keypath without its leading dot: `a.b[0]."c-d"`.
    fn block_path(&mut self) -> P {
        self.start(BLOCK_PATH);
        if self.at_name() || self.at(QNAME) || self.at(STRING) {
            self.bump();
        } else {
            return self.err_expected("an attribute path");
        }
        loop {
            if self.at(PATH) && self.glued_next() {
                self.bump();
            } else if self.at(DOT) && self.glued_next() {
                self.bump();
                if self.at_name() || self.at(QNAME) || self.at(STRING) {
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
    /// nested `path: { ... }`, separated by optional commas.
    fn attr_block(&mut self) -> P {
        self.expect(L_BRACE)?;
        while !self.at(R_BRACE) {
            self.start(ATTR_DECL);
            self.block_path()?;
            self.expect(COLON)?;
            if self.at(L_BRACE) {
                self.attr_block()?;
            } else {
                self.type_expr()?;
                while self.at(IDENT)
                    && matches!(
                        self.nth_text(0),
                        "required" | "computed" | "id" | "sensitive" | "nullable"
                    )
                    && !self.at_attr_decl()
                {
                    self.bump();
                }
                self.where_clause(true)?;
            }
            self.finish();
            if !self.eat(COMMA) && !self.at(R_BRACE) && !self.on_new_line() {
                return self.err_expected("`,`, a new line or `}`");
            }
        }
        self.bump();
        Ok(())
    }

    /// The next tokens start an attribute declaration: `path:`.
    fn at_attr_decl(&self) -> bool {
        (self.nth(0).is_name() || self.nth(0) == QNAME || self.nth(0) == STRING)
            && self.nth(1) == COLON
    }

    fn where_clause(&mut self, in_type: bool) -> P {
        if !self.at(WHERE_KW) {
            return Ok(());
        }
        self.start(WHERE_CLAUSE);
        self.bump();
        self.start(BODY);
        loop {
            self.lit()?;
            if !self.at(COMMA) || (in_type && self.nth_is_attr_decl_after_comma()) {
                break;
            }
            self.bump();
        }
        self.finish();
        self.finish();
        Ok(())
    }

    fn nth_is_attr_decl_after_comma(&self) -> bool {
        (self.nth(1).is_name() || self.nth(1) == QNAME || self.nth(1) == STRING)
            && self.nth(2) == COLON
    }

    /// `type := name | name(type, ...) | { name: type, ... } | STRING`
    fn type_expr(&mut self) -> P {
        self.start(TYPE_EXPR);
        match self.nth(0) {
            STRING => self.bump(),
            L_BRACE => {
                self.bump();
                while !self.at(R_BRACE) {
                    self.start(OBJECT_FIELD);
                    if self.at_name() || self.at(STRING) {
                        self.bump();
                    } else {
                        return self.err_expected("a field name");
                    }
                    self.expect(COLON)?;
                    self.type_expr()?;
                    self.finish();
                    if !self.eat(COMMA) {
                        break;
                    }
                }
                self.expect(R_BRACE)?;
            }
            k if k.is_name() || k == QNAME => {
                self.bump();
                if self.at(L_PAREN) && self.glued_next() {
                    self.bump();
                    loop {
                        self.type_expr()?;
                        if !self.eat(COMMA) {
                            break;
                        }
                    }
                    self.expect(R_PAREN)?;
                }
            }
            _ => return self.err_expected("a type"),
        }
        self.finish();
        Ok(())
    }

    // --- bodies and literals ---------------------------------------------

    fn body(&mut self) -> P {
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

    /// An atom starts here: a name, then `(` or a record's `{`, and (for
    /// `(`) the matching `)` is not followed by an operator that makes the
    /// call a term (`f(X) = Y`).
    fn at_atom(&self) -> bool {
        let k0 = self.nth(0);
        if !(k0.is_name() || k0 == QNAME) {
            return false;
        }
        match self.nth(1) {
            L_BRACE => k0 != QNAME,
            L_PAREN => {
                let mut nest = 0usize;
                let mut n = 1;
                loop {
                    match self.nth(n) {
                        L_PAREN | L_BRACKET | L_BRACE => nest += 1,
                        R_PAREN | R_BRACKET | R_BRACE => {
                            nest -= 1;
                            if nest == 0 {
                                break;
                            }
                        }
                        EOF => return true,
                        _ => {}
                    }
                    n += 1;
                }
                let after = self.nth(n + 1);
                let makes_term = matches!(
                    after,
                    EQ | EQ2
                        | NEQ
                        | LT
                        | LE
                        | GT
                        | GE
                        | IN_KW
                        | PLUS
                        | MINUS
                        | STAR
                        | SLASH
                        | PERCENT
                ) || (after == NOT_KW && self.nth(n + 2) == IN_KW);
                !makes_term
            }
            _ => false,
        }
    }

    fn lit(&mut self) -> P {
        if self.at(NOT_KW) && self.nth(1) == EXISTS_KW {
            self.start(LIT_NOT_EXISTS);
            self.bump();
            self.bump();
            self.expect(L_PAREN)?;
            self.body()?;
            self.expect(R_PAREN)?;
            self.finish();
            return Ok(());
        }
        if self.at(NOT_KW) {
            self.start(LIT_NOT);
            self.bump();
            if !self.at_atom() {
                return self.err_expected("an atom after `not`");
            }
            self.atom()?;
            self.finish();
            return Ok(());
        }
        if self.at_atom() {
            self.start(LIT_ATOM);
            self.atom()?;
            self.finish();
            return Ok(());
        }
        let cp = self.checkpoint();
        self.term(false)?;
        match self.nth(0) {
            EQ | EQ2 | NEQ | LT | LE | GT | GE => {
                self.start_at(cp, LIT_CMP);
                // `lo <= x <= hi` chains: each operator compares its neighbours.
                while matches!(self.nth(0), EQ | EQ2 | NEQ | LT | LE | GT | GE) {
                    self.bump();
                    self.term(false)?;
                }
            }
            IN_KW => {
                self.start_at(cp, LIT_IN);
                self.bump();
                self.term(false)?;
            }
            NOT_KW if self.nth(1) == IN_KW => {
                self.start_at(cp, LIT_NOT_IN);
                self.bump();
                self.bump();
                self.term(false)?;
            }
            _ => return self.err_expected("a comparison, `in` or `not in` after the term"),
        }
        self.finish();
        Ok(())
    }

    /// `name(args)` or `name{ field: term, ... }`.
    fn atom(&mut self) -> P {
        let name = self.nth_text(0);
        if self.nth(1) == L_BRACE {
            self.start(RECORD_ATOM);
            self.bump();
            self.bump();
            while !self.at(R_BRACE) {
                self.start(RECORD_FIELD);
                self.expect_name()?;
                self.expect(COLON)?;
                self.term(false)?;
                self.finish();
                if !self.eat(COMMA) {
                    break;
                }
            }
            self.expect(R_BRACE)?;
            self.finish();
            return Ok(());
        }
        self.start(ATOM);
        self.bump();
        self.arg_list(name)?;
        self.finish();
        Ok(())
    }

    fn arg_list(&mut self, callee: &str) -> P {
        self.start(ARG_LIST);
        self.expect(L_PAREN)?;
        let mut i = 0;
        while !self.at(R_PAREN) {
            self.term(addr_position(callee, i))?;
            i += 1;
            if !self.eat(COMMA) {
                break;
            }
        }
        self.expect(R_PAREN)?;
        self.finish();
        Ok(())
    }

    // --- terms ------------------------------------------------------------

    /// A term; `addr` allows `qname/ident` as an address.
    fn term(&mut self, addr: bool) -> P {
        self.expr(0, addr)
    }

    fn expr(&mut self, min_bp: u8, addr: bool) -> P {
        let cp = self.checkpoint();
        if self.at(MINUS) {
            self.start(UNARY_EXPR);
            self.bump();
            self.expr(5, false)?;
            self.finish();
        } else {
            self.primary(addr)?;
        }
        loop {
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
            self.expr(r, false)?;
            self.finish();
        }
        Ok(())
    }

    fn primary(&mut self, addr: bool) -> P {
        let k = self.nth(0);
        match k {
            INT | STRING | TRUE_KW | FALSE_KW | NULL_KW => self.leaf(LITERAL),
            VAR => self.leaf(VAR_REF),
            FIELD => self.leaf(FIELD_ACCESS),
            PATH => self.leaf(PATH_LIT),
            L_PAREN => {
                self.start(PAREN);
                self.bump();
                self.term(false)?;
                self.expect(R_PAREN)?;
                self.finish();
                Ok(())
            }
            L_BRACKET => self.list(),
            L_BRACE => self.object(),
            _ if k == QNAME || k.is_name() => {
                if self.nth(1) == L_PAREN {
                    let name = self.nth_text(0);
                    self.start(CALL);
                    self.bump();
                    self.arg_list(name)?;
                    self.finish();
                    return Ok(());
                }
                if addr && k == QNAME && self.nth(1) == SLASH && self.nth(2).is_name() {
                    self.start(ADDR);
                    self.bump();
                    self.bump();
                    self.bump();
                    self.finish();
                    return Ok(());
                }
                if self.nth(1) == DOT && self.glued(1) && self.nth(2) == VAR && self.glued(2) {
                    self.start(QNAME_VAR);
                    self.bump();
                    self.bump();
                    self.bump();
                    self.finish();
                    return Ok(());
                }
                self.leaf(NAME_REF)
            }
            _ => self.err_expected("a term"),
        }
    }

    fn leaf(&mut self, kind: SyntaxKind) -> P {
        self.start(kind);
        self.bump();
        self.finish();
        Ok(())
    }

    /// `[a, b]`, `[item | body]`, `[item ordered by key | body]`.
    fn list(&mut self) -> P {
        let cp = self.checkpoint();
        self.bump();
        if self.at(R_BRACKET) {
            self.start_at(cp, LIST);
            self.bump();
            self.finish();
            return Ok(());
        }
        self.term(false)?;
        if self.at(PIPE) || self.at_contextual("ordered") {
            self.start_at(cp, COMPREHENSION);
            if self.at_contextual("ordered") {
                self.bump();
                if !self.at_contextual("by") {
                    return self.err_expected("`by`");
                }
                self.bump();
                self.term(false)?;
            }
            self.expect(PIPE)?;
            self.body()?;
            self.expect(R_BRACKET)?;
            self.finish();
            return Ok(());
        }
        self.start_at(cp, LIST);
        while self.eat(COMMA) {
            if self.at(R_BRACKET) {
                break;
            }
            self.term(false)?;
        }
        self.expect(R_BRACKET)?;
        self.finish();
        Ok(())
    }

    fn object(&mut self) -> P {
        self.start(OBJECT);
        self.bump();
        while !self.at(R_BRACE) {
            self.start(OBJECT_FIELD);
            if self.at_name() || self.at(STRING) {
                self.bump();
            } else {
                return self.err_expected("an object key (a name or a string)");
            }
            self.expect(COLON)?;
            self.term(false)?;
            self.finish();
            if !self.eat(COMMA) {
                break;
            }
        }
        self.expect(R_BRACE)?;
        self.finish();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(src: &str) -> Vec<String> {
        parse(src).errors.into_iter().map(|e| e.message).collect()
    }

    #[test]
    fn lossless_even_with_errors() {
        for src in [
            "edition 2026.\np(a) :- q(X), X > 1. # c\n",
            "p(a :- .\nq(b).\n}}\n",
            "resource net.vpc main { cidr = \"x\" }\nq(b).",
        ] {
            assert_eq!(parse(src).syntax().to_string(), src);
        }
    }

    #[test]
    fn three_errors_three_diagnostics() {
        let src = "p(a) :- q(.\nok(1).\nr(b) :- .\nok(2).\nresource x { }.\n";
        assert_eq!(errors(src).len(), 3, "{:?}", errors(src));
    }

    #[test]
    fn a_missing_terminator_says_so() {
        let e = parse("p(a)\nq(b).\n").errors;
        assert_eq!(e.len(), 1);
        assert!(e[0].message.starts_with("expected `.`"), "{e:?}");
        assert!(e[0].hint.is_some());
    }

    #[test]
    fn a_call_compared_is_a_term() {
        let tree = parse("p(X) :- f(X) = 3, g(X).").syntax();
        let body: Vec<SyntaxKind> = tree
            .descendants()
            .filter(|n| matches!(n.kind(), LIT_CMP | LIT_ATOM))
            .map(|n| n.kind())
            .collect();
        assert_eq!(body, vec![LIT_CMP, LIT_ATOM]);
    }

    #[test]
    fn errors_inside_a_block_recover_at_the_brace() {
        let src = "module m {\n  p(a) :- .\n  q(b).\n}.\nr(c).\n";
        assert_eq!(errors(src).len(), 1, "{:?}", errors(src));
    }
}
