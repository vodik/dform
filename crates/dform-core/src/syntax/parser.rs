//! Recursive descent over the token stream into a lossless rowan tree
//! (docs/grammar.md). Statements are hand-written productions, terms a Pratt
//! parser. Names are parsed unresolved: a `CHAIN` is `name (.seg | [t])*`,
//! and what it denotes is the resolver's business (`syntax::resolve`).
//!
//! A statement's first token decides what it is (H-2): a name followed by
//! `(` is a fact or a rule, anything else starts with a keyword. A newline
//! outside `( )`, `[ ]` and an object's braces ends a statement, a block
//! entry or a literal of a `{ }` body; nothing continues a line. An error
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
    /// A header statement written after the body began (R-27): the
    /// statement parsed, and `fmt` moves it.
    pub misplaced: bool,
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

/// Parse one term, for an interpolation hole or an address: the tree is a
/// SOURCE_FILE holding the term (and an ERROR node for anything after it).
pub fn parse_term(src: &str) -> Parse {
    let mut p = Parser::new(src);
    p.start_root();
    p.nl.push(false);
    if p.term().is_ok() && p.nth(0) != EOF {
        let msg = format!("expected the end of the term, found {}", p.found());
        p.error_here(msg, None);
    }
    p.start(ERROR);
    while p.raw(0) != EOF {
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
/// The verb rule (R-57), for an `input` that gives rows with `=`: `=`
/// gives a value, `from` gives rows.
const VERB_RULE_ROWS: &str = "`=` gives a value and `from` gives rows: a value is `input p: T = d`, \
                              and rows are facts, `p(\"a\", 1)`, or `input p(a, b) from facts(..)`";

/// The verb rule (R-57), for an `input` that gives a value `from` a
/// document.
const VERB_RULE_FROM: &str = "`from` gives rows and `=` gives a value: a relation input names its \
                              columns, `input p(a, b) from facts(..)`, and a value is `input x: T = d`";

fn term_name(k: SyntaxKind) -> bool {
    k == IDENT
        || (k.is_keyword()
            && !matches!(
                k,
                NOT_KW | IN_KW | WHERE_KW | IF_KW | HAS_KW | TRUE_KW | FALSE_KW
            ))
}

/// Any word: a key, a path segment, a declared name.
fn word(k: SyntaxKind) -> bool {
    k == IDENT || k.is_keyword()
}

/// A statement of a file's header (R-27): what the program takes, before
/// its body.
fn header_stmt(k: SyntaxKind) -> bool {
    matches!(k, KEY_KW | INPUT_KW)
}

fn is_cmp(k: SyntaxKind) -> bool {
    matches!(k, EQ | EQ2 | NEQ | LT | LE | GT | GE)
}

/// Statements of an earlier surface, and what each is spelled now.
fn old_spelling(word: &str) -> Option<&'static str> {
    Some(match word {
        "when" | "for" => {
            "statement groups are gone (H-4): put `where B` on each statement, or gate a group \
             of resources by an `instance` with a `where` clause"
        }
        "with" => "`with k = v` is spelled `set k = v`",
        "constraint" => "`constraint` is spelled `deny` (H-8)",
        "apply" => "`apply pack` is spelled `use pack`",
        "component_def" => "`component_def` is spelled `component`",
        "module" => {
            "`module` is gone (R-65): a module is a file, named by its path and brought in \
             with `use PATH`; a thing copied with inputs is `component NAME { .. }`, made with \
             `instance`"
        }
        "policy" | "policy_pack" => {
            "`policy` is gone (R-65): a policy pack is a module, a file of `set`, `deny` and \
             `warn` statements, applied with `use PATH`"
        }
        "apply_policy" => "`apply_policy` is spelled `use`",
        "import" => {
            "`import` is gone (R-65): `use PATH` brings in a module by its path from the \
             project root, `use modules.net` for modules/net.df"
        }
        "export" => {
            "`export` is gone (R-65): a module's items are public, and a component's outputs \
             are its public face"
        }
        "unique" => "`unique` is gone: one value per key is what the attribute aggregate enforces",
        "contributes" => {
            "`contributes` is gone (R-5): a write needs no grant, delete the line; a module's \
             relation reaches the stack through an output"
        }
        "scenario" => {
            "`scenario` is gone (R-32): the program's denies are its tests, and `dform test` \
             runs them over the inputs' values; a what-if plan is `plan --set k=v`"
        }
        "stack" => {
            "the `stack` statement is gone (R-29): a file under stacks/ is a stack named after \
             itself, `key env: T` keys it, and dform.toml's `[stacks.NAME]` holds its settings"
        }
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
    /// and in blocks and `{ }` bodies, false inside brackets and objects.
    nl: Vec<bool>,
    /// The token whose preceding newline a separator has taken.
    nl_eaten: Option<usize>,
    /// Byte offset of the statement being parsed, for a hint that prints it.
    stmt_start: usize,
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
            nl_eaten: None,
            stmt_start: 0,
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

    /// The `n`th significant token ahead, newlines aside.
    fn raw(&self, n: usize) -> SyntaxKind {
        self.nth_index(n).map_or(EOF, |i| self.toks[i].kind)
    }

    /// The `n`th token ahead; the next one is `NEWLINE` when a newline
    /// before it ends what is being parsed.
    fn nth(&self, n: usize) -> SyntaxKind {
        if n == 0 && self.nl_stop() {
            return NEWLINE;
        }
        self.raw(n)
    }

    fn nth_text(&self, n: usize) -> &'a str {
        self.nth_index(n)
            .map_or("", |i| &self.src[self.toks[i].start..self.toks[i].end])
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

    /// The next token is on a new line where a newline ends the construct,
    /// and no separator has taken that newline.
    fn nl_stop(&self) -> bool {
        self.nl.last().copied().unwrap_or(true)
            && self.on_new_line()
            && self.nl_eaten != self.nth_index(0)
    }

    /// Take the newline before the next token as a separator.
    fn eat_nl(&mut self) -> bool {
        if self.nth(0) == NEWLINE {
            self.nl_eaten = self.nth_index(0);
            true
        } else {
            false
        }
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
            NEWLINE => "the end of the line".to_string(),
            ERROR_TOKEN => format!("unknown character `{}`", self.nth_text(0)),
            k if k.is_keyword() => format!("keyword `{}`", self.nth_text(0)),
            IDENT => format!("`{}`", self.nth_text(0)),
            STRING | INT | RANK => {
                format!("{} `{}`", self.nth(0).describe(), self.nth_text(0))
            }
            k => k.describe().to_string(),
        }
    }

    fn error_here(&mut self, message: String, hint: Option<String>) {
        let (start, end) = match self.nth_index(0) {
            Some(i) if self.nth(0) != NEWLINE => (self.toks[i].start, self.toks[i].end),
            // At the end of a line: the point after the last token.
            _ => {
                let at = self.toks[..self.pos.min(self.toks.len())]
                    .iter()
                    .rev()
                    .find(|t| !t.kind.is_trivia())
                    .map_or(self.src.len(), |t| t.end);
                (at, at)
            }
        };
        self.errors.push(ParseError {
            start,
            end,
            message,
            hint,
            misplaced: false,
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
            NEWLINE => {
                return Some(
                    "a newline ends a statement: a body of several lines is `where { .. }`, one \
                     literal per line; a long term wraps inside brackets"
                        .to_string(),
                );
            }
            NECK => {
                return Some("a rule is `head where body`: `:-` is spelled `where`".to_string());
            }
            DOT if matches!(self.raw(1), EOF) || self.on_new_line_at(1) => {
                return Some(
                    "a statement ends at the end of its line; there is no `.` terminator"
                        .to_string(),
                );
            }
            DOT => {
                return Some(
                    "a path that is data is a string (`\"cidr\"`, H-12); a `.` is member access"
                        .to_string(),
                );
            }
            SLASH if self.raw(1) == SLASH => {
                return Some("a comment starts with `#`".to_string());
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

    /// `name (.name)*`: a type, an extern, a relation.
    fn dotted(&mut self, what: &str) -> P {
        if !word(self.nth(0)) {
            return self.err_expected(what);
        }
        self.bump();
        while self.at(DOT) && word(self.raw(1)) {
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
            match self.raw(0) {
                EOF => break,
                R_BRACE if nest == 0 && in_block => break,
                _ if skipped && nest == 0 && self.on_new_line() => break,
                k if skipped && self.on_new_line() && self.at_col0() && k.is_stmt_keyword() => {
                    break;
                }
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
        // A file's header, `import`, `key` and `input` lines, comes before
        // its body (R-27): the byte offset of the body's first statement.
        let mut body: Option<usize> = None;
        loop {
            self.eat_nl();
            match self.nth(0) {
                EOF => break,
                R_BRACE if in_block => break,
                _ => {}
            }
            let depth = self.depth;
            let nl = self.nl.len();
            self.flush_trivia();
            let start = self.pos;
            self.stmt_start = self.toks.get(start).map_or(self.src.len(), |t| t.start);
            let header = header_stmt(self.nth(0)) && !self.at_head();
            if !in_block && !header && self.nth(0) != EDITION_KW {
                body.get_or_insert(self.stmt_start);
            }
            let misplaced = body
                .filter(|_| !in_block && header)
                .map(|b| self.misplaced(b));
            let ok = self.stmt().and_then(|()| self.stmt_end(in_block));
            // Only a statement that parsed is out of place.
            if ok.is_ok()
                && let Some(e) = misplaced
            {
                self.errors.push(e);
            }
            if ok.is_err() {
                self.close_to(depth);
                self.nl.truncate(nl);
                self.recover(start, in_block);
            }
        }
    }

    /// A header statement after the body's first statement (at byte
    /// `body`): an error that says to move it.
    fn misplaced(&self, body: usize) -> ParseError {
        let i = self.nth_index(0).expect("a statement");
        let name = self.nth_text(1);
        let what = match self.raw(2) {
            L_PAREN => format!("{} {name}(..)", self.nth_text(0)),
            _ => format!("{} {name}", self.nth_text(0)),
        };
        let line = self.src[..body].matches('\n').count() + 1;
        ParseError {
            start: self.toks[i].start,
            end: self.toks[i].end,
            message: format!(
                "`{what}` is a header statement: move it above the body's first statement, \
                 line {line}"
            ),
            hint: Some(
                "a file is `edition`, then its header (`key`, `input`), then its body; \
                 `dform fmt` moves it"
                    .to_string(),
            ),
            misplaced: true,
        }
    }

    /// A statement ends at a newline, the `}` of its block, or the file's end.
    fn stmt_end(&mut self, in_block: bool) -> P {
        match self.nth(0) {
            EOF | NEWLINE => Ok(()),
            R_BRACE if in_block => Ok(()),
            _ => self.err_expected("the end of the line"),
        }
    }

    /// The statement ahead is `NAME (.NAME)* (`: a fact or a rule.
    fn at_head(&self) -> bool {
        if !word(self.nth(0)) {
            return false;
        }
        let mut i = 1;
        loop {
            match self.raw(i) {
                L_PAREN => return true,
                DOT if word(self.raw(i + 1)) => i += 2,
                _ => return false,
            }
        }
    }

    fn stmt(&mut self) -> P {
        let k = self.nth(0);
        // The first token decides: a name followed by `(` is a fact or a
        // rule, whatever the name.
        if self.at_head() {
            return self.rule();
        }
        match k {
            EDITION_KW => self.simple(EDITION, |p| p.expect(INT)),
            // `provider aws`: a block with no entries is left out (R-26).
            PROVIDER_KW => self.simple(PROVIDER, |p| {
                p.expect_word()?;
                p.opt_block()?;
                p.opt_clause()
            }),
            INPUT_KW
                if self.raw(1) == IDENT
                    && self.nth_text(1) == "relation"
                    && self.raw(2) == IDENT =>
            {
                self.bump();
                let msg = format!(
                    "expected `:` or `(` after the input's name, found `{}`",
                    self.nth_text(1)
                );
                self.error_here(
                    msg,
                    Some(
                        "`input relation p(cols) from ..` is spelled `input p(cols) from ..`, \
                         and `from file(..)` is `from facts(..)`"
                            .to_string(),
                    ),
                );
                Err(Bail)
            }
            INPUT_KW if self.raw(2) == L_PAREN => self.simple(INPUT_RELATION, |p| {
                p.expect_word()?;
                p.columns(true)?;
                if p.at(EQ) {
                    let msg = format!("expected `from`, found {}", p.found());
                    p.error_here(msg, Some(VERB_RULE_ROWS.to_string()));
                    return Err(Bail);
                }
                if !p.at_contextual("from") {
                    return p.err_expected("`from`");
                }
                p.bump();
                p.term().map(drop)
            }),
            // `key k: T`: an input the target gives, which selects the
            // deployment (R-29). It has no relation form.
            KEY_KW if self.raw(2) == L_PAREN => {
                self.bump();
                self.bump();
                let msg = format!("expected `:` after the key's name, found {}", self.found());
                self.error_here(
                    msg,
                    Some(
                        "a key is a value, `key env: environment`; a relation is not a key".into(),
                    ),
                );
                Err(Bail)
            }
            INPUT_KW | KEY_KW => self.simple(INPUT, |p| {
                p.expect_word()?;
                if !p.at(COLON) {
                    let hint = if p.at_contextual("from") {
                        Some(VERB_RULE_FROM.to_string())
                    } else if p.at(EQ) {
                        Some(VERB_RULE_ROWS.to_string())
                    } else {
                        None
                    };
                    let msg = format!(
                        "expected `:` or `(` after the input's name, found {}",
                        p.found()
                    );
                    p.error_here(msg, hint);
                    return Err(Bail);
                }
                p.bump();
                p.type_expr()?;
                if p.at_contextual("from") {
                    let msg = format!("expected `=` or the end of the line, found {}", p.found());
                    p.error_here(msg, Some(VERB_RULE_FROM.to_string()));
                    return Err(Bail);
                }
                if p.eat(EQ) {
                    p.term()?;
                }
                p.refinement(false)
            }),
            OUTPUT_KW => self.simple(OUTPUT_DECL, |p| {
                p.expect_word()?;
                if p.eat(COLON) {
                    p.type_expr()?;
                }
                if p.eat(EQ) {
                    p.term()?;
                }
                p.opt_where_body()
            }),
            LET_KW => self.simple(LET, |p| {
                p.expect_word()?;
                p.expect(EQ)?;
                p.term()?;
                p.eat(RANK);
                p.opt_where_body()
            }),
            SET_KW => self.simple(SET, |p| {
                if !term_name(p.nth(0)) {
                    return p.err_expected("what the contribution sets: `r.path`, an input");
                }
                p.chain()?;
                if !(p.eat(EQ) || p.eat(PLUS_EQ)) {
                    return p.err_expected("`=` or `+=`");
                }
                p.term()?;
                p.eat(RANK);
                p.opt_where_body()
            }),
            EXTERN_KW => self.simple(EXTERN, |p| {
                p.dotted("an extern name")?;
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
                if p.at_contextual("persist") {
                    p.bump();
                }
                Ok(())
            }),
            // `type NAME = TYPE`: an alias.
            TYPE_KW if self.raw(2) == EQ => self.simple(TYPE_ALIAS, |p| {
                p.expect_word()?;
                p.expect(EQ)?;
                p.type_expr()
            }),
            TYPE_KW => self.simple(TYPE_DECL, |p| {
                p.dotted("a type name")?;
                p.attr_block()
            }),
            DECL_KW => self.simple(DECL, |p| {
                if p.at(TYPE_KW) {
                    let hint = "an open type is not supported: declare the type's attributes \
                                with a `type` block"
                        .to_string();
                    p.error_here("expected a relation name, found `type`".into(), Some(hint));
                    return Err(Bail);
                }
                p.dotted("a relation name")?;
                if !p.at(L_PAREN) {
                    let hint = p.at(SLASH).then(|| {
                        "a relation is declared by its columns: `decl p(a, b)`".to_string()
                    });
                    let msg = format!("expected `(`, found {}", p.found());
                    p.error_here(msg, hint);
                    return Err(Bail);
                }
                p.columns(true)?;
                if p.at_contextual("mixed") {
                    p.bump();
                }
                Ok(())
            }),
            // `component NAME { .. }`: a component declared as an item.
            COMPONENT_KW => self.simple(COMPONENT, |p| {
                p.expect_word()?;
                p.stmt_block()
            }),
            // `use PATH [as NAME] [{ .. }] [where B]` (R-65): a module
            // imported once, or a stack's deployments.
            USE_KW => self.simple(USE, |p| {
                p.dotted("a module's path")?;
                if p.at_contextual("as") {
                    p.bump();
                    p.expect_word()?;
                }
                p.opt_block()?;
                p.opt_clause()
            }),
            // `instance PATH NAME [{ .. }] [where B]`: a named copy (R-65).
            INSTANCE_KW => self.simple(INSTANCE, |p| {
                p.dotted("a component's path")?;
                if word(p.nth(0)) && !matches!(p.nth(0), WHERE_KW | IF_KW) {
                    p.bump();
                } else {
                    let msg = format!("expected the instance's name, found {}", p.found());
                    p.error_here(
                        msg,
                        Some(
                            "a copy is named, `instance network blue`; a module is imported \
                             once by `use`, under its name"
                                .into(),
                        ),
                    );
                    return Err(Bail);
                }
                p.opt_block()?;
                p.opt_clause()
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
                p.block()?;
                p.opt_clause()
            }),
            SETTINGS_KW => self.simple(SETTINGS, |p| {
                if word(p.nth(0)) || p.at(STRING) {
                    p.bump();
                } else {
                    let hint = matches!(p.nth(0), DOT | L_BRACKET).then(|| {
                        "a contribution to a settings row is `set settings[e].path = t`".to_string()
                    });
                    let msg = format!(
                        "expected a settings row's name (a name or a string), found {}",
                        p.found()
                    );
                    p.error_here(msg, hint);
                    return Err(Bail);
                }
                p.eat(RANK);
                p.block()?;
                p.opt_clause()
            }),
            DENY_KW | WARN_KW => self.simple(CHECK, |p| {
                p.expect(STRING)?;
                if p.at(L_BRACE) {
                    p.with_nl(false, |p| p.object().map(drop))?;
                }
                p.opt_where_body()
            }),
            _ => {
                let text = self.nth_text(0);
                let hint = if k == IDENT
                    && let Some(h) = old_spelling(text)
                {
                    Some(h.to_string())
                } else if matches!(k, IF_KW | WHERE_KW) {
                    Some("`where` goes on the line of the head it guards, after it".to_string())
                } else if term_name(k) && matches!(self.raw(1), EQ | PLUS_EQ) {
                    Some(format!("a value is `let {text} = t`"))
                } else if term_name(k) && matches!(self.raw(1), DOT | L_BRACKET) {
                    Some("a contribution to another block's field is `set r.path = t`".to_string())
                } else {
                    self.hint()
                };
                let msg = format!("expected a statement, found {}", self.found());
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

    /// `p(args) [rank] [where body]`: a fact or a rule.
    fn rule(&mut self) -> P {
        let cp = self.checkpoint();
        self.chain_term()?;
        self.eat(RANK);
        let has_body = self.at(WHERE_KW);
        self.start_at(cp, if has_body { RULE } else { FACT });
        self.opt_where_body()?;
        self.finish();
        Ok(())
    }

    /// `[where body]` on the line the statement is on.
    fn opt_where_body(&mut self) -> P {
        if self.at(NECK) {
            return self.err_expected("`where` or the end of the line");
        }
        if self.at(IF_KW) {
            return self.if_is_gone();
        }
        if self.at_contextual("check") {
            let msg = format!(
                "expected `where` or the end of the line, found {}",
                self.found()
            );
            self.error_here(
                msg,
                Some(
                    "`check` refines an input's or an attribute's type; a clause is `where`".into(),
                ),
            );
            return Err(Bail);
        }
        if self.eat(WHERE_KW) {
            self.body()?;
        }
        Ok(())
    }

    /// `[where body]` after a block: the block is the head (R-1).
    fn opt_clause(&mut self) -> P {
        if self.at(IF_KW) {
            return self.if_is_gone();
        }
        if self.at(WHERE_KW) {
            self.start(CLAUSE);
            self.bump();
            self.body()?;
            self.finish();
        }
        Ok(())
    }

    /// `if` where a clause goes: the error prints the statement with its
    /// clause spelled `where`.
    fn if_is_gone(&mut self) -> P {
        let i = self.nth_index(0).expect("at `if`");
        let head = self.head_text(self.toks[i].start);
        let body = self.rest_of_clause(i + 1);
        let msg = "expected `where` or the end of the line, found keyword `if`".to_string();
        let hint = format!("the clause word is `where` (R-1): `{head} where {body}`");
        self.error_here(msg, Some(hint));
        Err(Bail)
    }

    /// The statement's text up to byte `end`, for a hint: a block that
    /// spans lines is `{ .. }`.
    fn head_text(&self, end: usize) -> String {
        let head = self.src[self.stmt_start..end].trim_end();
        match head.find('{') {
            Some(open) if head.contains('\n') => format!("{} {{ .. }}", head[..open].trim_end()),
            _ => head.to_string(),
        }
    }

    /// The text of a clause's body starting after token `from`, for a
    /// hint: the rest of its line, or `{ .. }` for a body of several lines.
    fn rest_of_clause(&self, from: usize) -> String {
        let start = self.toks[from..]
            .iter()
            .find(|t| !t.kind.is_trivia())
            .map_or(self.src.len(), |t| t.start);
        let line = self.src[start..].split('\n').next().unwrap_or("");
        // A comment on the line is not part of the body.
        let line = match lexer::lex(line).iter().find(|t| t.kind == COMMENT) {
            Some(c) => &line[..c.start],
            None => line,
        };
        let line = line.trim();
        // A clause inside a one-line block runs into its entries.
        let closers = line.matches('}').count() > line.matches('{').count();
        if line == "{" {
            "{ .. }".to_string()
        } else if closers {
            "..".to_string()
        } else {
            line.to_string()
        }
    }

    /// `(name [: type], ...)`: a declaration's columns, a table's.
    fn columns(&mut self, types_optional: bool) -> P {
        self.expect(L_PAREN)?;
        self.with_nl(false, |p| {
            loop {
                p.start(BIND_ARG);
                p.expect_word()?;
                if types_optional {
                    if p.eat(COLON) {
                        p.type_expr()?;
                    }
                } else {
                    p.expect(COLON)?;
                    p.type_expr()?;
                }
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

    /// After an entry of a block or a literal of a `{ }` body: `,`, a
    /// newline, or the closing `}`.
    fn sep(&mut self) -> P {
        if self.eat(COMMA) {
            self.eat_nl();
            return Ok(());
        }
        if self.eat_nl() || self.at(R_BRACE) {
            return Ok(());
        }
        self.err_expected("`,`, a new line or `}`")
    }

    /// `{ entry* }` of a resource, settings, instance or provider:
    /// entries separated by a newline or a comma. Its clause follows it.
    fn block(&mut self) -> P {
        self.start(BLOCK);
        let open = self.nth_index(0);
        self.expect(L_BRACE)?;
        self.with_nl(true, |p| {
            p.eat_nl();
            while !p.at(R_BRACE) {
                let clause_word = matches!(p.nth(0), IF_KW | WHERE_KW)
                    || (p.at_contextual("for") && !matches!(p.raw(1), EQ | PLUS_EQ | DOT | L_BRACKET));
                if clause_word {
                    let i = p.nth_index(0).expect("a token");
                    let head = p.src[p.stmt_start..open.map_or(p.stmt_start, |o| p.toks[o].start)]
                        .trim_end();
                    let body = p.rest_of_clause(i + 1);
                    let msg = format!("expected an entry or `}}`, found {}", p.found());
                    p.error_here(
                        msg,
                        Some(format!(
                            "a block's clause follows the block (R-1): `{head} {{ .. }} where {body}`"
                        )),
                    );
                    return Err(Bail);
                }
                if p.at(EOF) {
                    return p.err_expected("`}`");
                }
                p.assign()?;
                p.sep()?;
            }
            Ok(())
        })?;
        self.bump();
        self.finish();
        Ok(())
    }

    /// A block that may be left out when it has no entries: `provider
    /// aws`, `instance network blue` (R-26).
    fn opt_block(&mut self) -> P {
        if self.at(L_BRACE) {
            self.block()
        } else {
            Ok(())
        }
    }

    /// `path = term [rank]`, `path += term [rank]`, or `path [rank]`
    /// alone: the pun `path = SEG`, SEG its last segment (R-33).
    fn assign(&mut self) -> P {
        self.start(ASSIGN);
        self.block_path()?;
        if self.eat(EQ) || self.eat(PLUS_EQ) {
            self.term()?;
        } else if !matches!(self.nth(0), RANK | COMMA | R_BRACE | NEWLINE) {
            return self.err_expected("`=`, `+=` or the end of the entry");
        }
        self.eat(RANK);
        self.finish();
        Ok(())
    }

    /// A path in a block: `a.b[0]."c-d"`.
    fn block_path(&mut self) -> P {
        self.start(BLOCK_PATH);
        if word(self.nth(0)) || self.at(STRING) {
            self.bump();
        } else {
            return self.err_expected("an attribute path");
        }
        loop {
            if self.at(DOT) {
                self.bump();
                if word(self.nth(0)) || self.at(STRING) {
                    self.bump();
                } else {
                    return self.err_expected("a path segment");
                }
            } else if self.at(L_BRACKET) {
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

    /// `{ attrdecl* }` of a user type: `path: type flag* [check body]` or a
    /// nested `path: { ... }`, separated by a newline or a comma.
    fn attr_block(&mut self) -> P {
        self.expect(L_BRACE)?;
        self.with_nl(true, |p| {
            p.eat_nl();
            while !p.at(R_BRACE) {
                if p.at(EOF) {
                    return p.err_expected("`}`");
                }
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
                    {
                        p.bump();
                    }
                    p.refinement(true)?;
                }
                p.finish();
                p.sep()?;
            }
            Ok(())
        })?;
        self.bump();
        Ok(())
    }

    /// The next tokens start an attribute declaration: `path:`.
    fn at_attr_decl(&self) -> bool {
        (word(self.nth(0)) || self.nth(0) == STRING) && self.raw(1) == COLON
    }

    /// `[check body]`: a refinement of an input's or an attribute's type.
    fn refinement(&mut self, in_type: bool) -> P {
        if matches!(self.nth(0), WHERE_KW | IF_KW) {
            let i = self.nth_index(0).expect("a token");
            let at = self.toks[i].start;
            // In a type block, the attribute's own line.
            let from = if in_type {
                self.src[..at].rfind('\n').map_or(0, |n| n + 1)
            } else {
                self.stmt_start
            };
            let head = self.src[from..at].trim();
            let body = self.rest_of_clause(i + 1);
            let msg = format!(
                "expected `check` or the end of the line, found {}",
                self.found()
            );
            self.error_here(
                msg,
                Some(format!(
                    "a refinement is spelled `check` (R-1): `{head} check {body}`"
                )),
            );
            return Err(Bail);
        }
        if !self.at_contextual("check") {
            return Ok(());
        }
        self.start(REFINEMENT);
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
        (word(self.raw(1)) || self.raw(1) == STRING) && self.raw(2) == COLON
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
                if self.at(L_PAREN) {
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

    /// `{ lit (, | newline) lit ... }` or `lit, lit, ...` on one line.
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
            p.eat_nl();
            while !p.at(R_BRACE) {
                if p.at(EOF) {
                    return p.err_expected("`}`");
                }
                p.lit()?;
                p.sep()?;
            }
            Ok(())
        })?;
        self.bump();
        self.finish();
        Ok(())
    }

    fn lit(&mut self) -> P {
        if self.at(NOT_KW) && self.raw(1) == L_BRACE {
            self.start(LIT_NOT_BLOCK);
            self.bump();
            self.body_block()?;
            self.finish();
            return Ok(());
        }
        if self.at(NOT_KW) && self.raw(1) != IN_KW {
            self.start(LIT_NOT);
            self.bump();
            self.lit1()?;
            self.finish();
            return Ok(());
        }
        self.lit1()
    }

    fn lit1(&mut self) -> P {
        if self.at(HAS_KW) {
            self.start(LIT_HAS);
            self.bump();
            self.term()?;
            self.finish();
            return Ok(());
        }
        if self.at(IDENT)
            && matches!(self.nth_text(0), "exists" | "some")
            && (term_name(self.raw(1)) || self.raw(1) == STRING)
            && !matches!(self.raw(1), L_PAREN)
        {
            let hint = if self.nth_text(0) == "exists" {
                "`exists R` is `R in T` (H-9): a resource named in scope is its address"
            } else {
                "`some x in e` is `x in e`; `some i, x in e` is `x = e[i]` (H-9)"
            };
            let msg = format!("expected a literal, found {}", self.found());
            self.error_here(msg, Some(hint.to_string()));
            return Err(Bail);
        }
        let cp = self.checkpoint();
        let kind = self.term()?;
        match self.nth(0) {
            k if is_cmp(k) => {
                self.start_at(cp, LIT_CMP);
                // `lo <= x <= hi` chains: each operator compares its neighbours.
                while is_cmp(self.nth(0)) {
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
            NOT_KW if self.raw(1) == IN_KW => {
                self.start_at(cp, LIT_NOT_IN);
                self.bump();
                self.bump();
                self.term()?;
            }
            _ if kind == CALL => self.start_at(cp, LIT_ATOM),
            _ if kind == CHAIN => self.start_at(cp, LIT_TRUTH),
            _ => return self.err_expected("a comparison, `in` or `not in` after the term"),
        }
        self.finish();
        Ok(())
    }

    /// `(term, ...)` or `(name: term, ...)`.
    fn arg_list(&mut self) -> P {
        self.start(ARG_LIST);
        self.expect(L_PAREN)?;
        self.with_nl(false, |p| {
            while !p.at(R_PAREN) {
                if word(p.nth(0)) && p.raw(1) == COLON {
                    p.start(NAMED_ARG);
                    p.bump();
                    p.bump();
                    p.term()?;
                    p.finish();
                } else {
                    p.term()?;
                }
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

    /// A term; returns the kind of its outermost node. `lo..hi` and
    /// `lo..=hi` bind loosest (R-56); the resolver takes a range only
    /// after `in`.
    fn term(&mut self) -> P<SyntaxKind> {
        let cp = self.checkpoint();
        let kind = self.expr(0)?;
        if !matches!(self.nth(0), DOT2 | DOT2_EQ) {
            return Ok(kind);
        }
        self.start_at(cp, RANGE);
        self.bump();
        self.expr(0)?;
        self.finish();
        Ok(RANGE)
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
            INT | STRING | TRUE_KW | FALSE_KW => self.leaf(LITERAL),
            L_PAREN => {
                // `(t)` groups; `(a, b, ..)` is a tuple pattern (R-58).
                let cp = self.checkpoint();
                self.bump();
                let kind = self.with_nl(false, |p| {
                    p.term()?;
                    if !p.at(COMMA) {
                        p.start_at(cp, PAREN);
                        p.expect(R_PAREN)?;
                        return Ok(PAREN);
                    }
                    p.start_at(cp, TUPLE);
                    p.bump();
                    p.term()?;
                    while p.eat(COMMA) {
                        if p.at(R_PAREN) {
                            break;
                        }
                        p.term()?;
                    }
                    p.expect(R_PAREN)?;
                    Ok(TUPLE)
                })?;
                self.finish();
                Ok(kind)
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

    /// A chain, then a `(` makes it a call.
    fn chain_term(&mut self) -> P<SyntaxKind> {
        let cp = self.checkpoint();
        self.chain()?;
        if self.at(L_PAREN) {
            self.start_at(cp, CALL);
            self.arg_list()?;
            self.finish();
            return Ok(CALL);
        }
        if self.at(L_BRACE) && word(self.raw(1)) && self.raw(2) == COLON {
            let msg = format!("expected the end of the term, found {}", self.found());
            self.error_here(
                msg,
                Some("a record atom `p{a: x}` is spelled with named arguments, `p(a: x)`".into()),
            );
            return Err(Bail);
        }
        Ok(CHAIN)
    }

    /// `name (.seg | [t, ...])*`.
    fn chain(&mut self) -> P {
        self.start(CHAIN);
        self.bump();
        loop {
            if self.at(DOT) {
                self.bump();
                if word(self.nth(0)) || self.at(STRING) {
                    self.bump();
                } else {
                    return self.err_expected("a name after `.`");
                }
            } else if self.at(L_BRACKET) {
                self.start(INDEX);
                self.bump();
                self.with_nl(false, |p| {
                    loop {
                        // `[k=v, ..]`: a stack's deployment by its keys
                        // (R-65).
                        if word(p.nth(0)) && p.raw(1) == EQ {
                            p.start(NAMED_ARG);
                            p.bump();
                            p.bump();
                            p.term()?;
                            p.finish();
                        } else {
                            p.term()?;
                        }
                        if !p.eat(COMMA) {
                            break;
                        }
                    }
                    p.expect(R_BRACKET)
                })?;
                self.finish();
            } else {
                break;
            }
        }
        self.finish();
        Ok(())
    }

    /// `[a, b]`, `[item | body]`.
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
            if p.at(PIPE) {
                p.start_at(cp, COMPREHENSION);
                p.bump();
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

    fn hints(src: &str) -> Vec<String> {
        parse(src)
            .errors
            .into_iter()
            .map(|e| e.hint.unwrap_or_default())
            .collect()
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
            "edition 2026\np(a) where q(x), x > 1 # c\n",
            "p(a if\nq(b)\n}}\n",
            "resource net.vpc main { cidr = \"x\" }\nq(b)",
        ] {
            assert_eq!(parse(src).syntax().to_string(), src);
        }
    }

    #[test]
    fn three_errors_three_diagnostics() {
        let src = "p(a) where q(]\nok(1)\nr(b) where ,\nok(2)\nresource x { }\nok(3)\n";
        assert_eq!(errors(src).len(), 3, "{:?}", errors(src));
    }

    #[test]
    fn a_newline_ends_a_statement_and_nothing_continues_it() {
        assert!(errors("p(a)\nq(b)\nr(c) where {\n  q(c)\n  p(c)\n}\n").is_empty());
        let e = parse("p(a) q(b)\n").errors;
        assert_eq!(e.len(), 1);
        assert!(
            e[0].message.starts_with("expected the end of the line"),
            "{e:?}"
        );
        for src in [
            "r(c) where q(c),\n  p(c)\n",
            "r(c) where\n  q(c)\n",
            "let x = 1 +\n  2\n",
            "resource t a {\n  b =\n    1\n}\n",
        ] {
            let e = parse(src).errors;
            assert!(!e.is_empty(), "{src}");
            assert!(
                e[0].message.ends_with("found the end of the line"),
                "{src}: {e:?}"
            );
        }
        // Inside brackets a newline is whitespace.
        assert!(errors("p(a,\n  b)\nlet x = (1 +\n  2)\n").is_empty());
    }

    #[test]
    fn an_old_terminator_says_so() {
        let e = parse("p(a).\nq(b).\n").errors;
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].hint.as_deref().unwrap().contains("no `.` terminator"));
        let e = parse("p(a) :- q(a)\n").errors;
        assert!(
            e[0].hint.as_deref().unwrap().contains("spelled `where`"),
            "{e:?}"
        );
    }

    #[test]
    fn the_first_token_decides() {
        let src = "let k = 1 where p(1)\nset r.tags = {} where r in resource\ndeny \"m\" { a } where p(a)\n\
                   p(x) where { q(x)\n r(x) }\nlet cfg = settings[env]\ndeny(\"m\") where q(1)\n\
                   input(\"a\", 1)\nfor(1)\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(
            kinds(src, &[LET, SET, CHECK, RULE, FACT]),
            vec![LET, SET, CHECK, RULE, LET, RULE, FACT, FACT]
        );
    }

    #[test]
    fn old_spellings_name_the_new_one() {
        for (src, hint) in [
            ("when p(1) { q(1) }\n", "statement groups are gone"),
            ("with env = \"prod\"\n", "`set k = v`"),
            ("constraint \"m\" where p(1)\n", "spelled `deny`"),
            ("apply baseline\n", "`use pack`"),
            ("module network {}\n", "a module is a file"),
            ("policy baseline {}\n", "a policy pack is a module"),
            ("import \"modules/net.df\"\n", "`use modules.net`"),
            ("export type t\n", "`export` is gone (R-65)"),
            ("k = 1\n", "`let k = t`"),
            ("r.tags = {}\n", "`set r.path = t`"),
            ("decl p/2\n", "`decl p(a, b)`"),
            (
                "component p {\n  contributes t.tags\n}\n",
                "`contributes` is gone",
            ),
            ("stack shop[env] {}\n", "`key env: T` keys it"),
            (
                "scenario prod {\n  set env = \"prod\"\n}\n",
                "`scenario` is gone (R-32)",
            ),
            ("key p(a) from csv(\"p.csv\")\n", "a relation is not a key"),
            (
                "input relation p/2 from file(\"x\")\n",
                "`input p(cols) from ..`",
            ),
            ("p(x) where exists x\n", "`R in T`"),
            ("p(x) where some x in [1]\n", "`x in e`"),
            ("p(x) where q(x), x == .cidr\n", "is a string"),
            ("p(x) where q{a: x}\n", "`p(a: x)`"),
            (
                "resource t a {\n  for q(x)\n  b = x\n}\n",
                "`resource t a { .. } where q(x)`",
            ),
            (
                "resource t a {\n  if q(x), r(x)\n  b = x\n}\n",
                "`resource t a { .. } where q(x), r(x)`",
            ),
            ("p(x) if q(x) # c\n", "`p(x) where q(x)`"),
            ("let k = 1 if {\n  q(1)\n}\n", "`let k = 1 where { .. }`"),
            ("instance m i {} if p(1)\n", "`instance m i {} where p(1)`"),
            ("input k: int = 1 where k > 0\n", "spelled `check`"),
            ("type t.u { a: int where a > 0 }\n", "spelled `check`"),
            ("let k = 1 check k > 0\n", "a clause is `where`"),
            ("// a comment\n", "`#`"),
        ] {
            let h = hints(src);
            assert!(h.iter().any(|x| x.contains(hint)), "{src}: {h:?}");
        }
    }

    #[test]
    fn an_alias_is_not_a_type_block() {
        let src = "type env = enum(\"a\")\ntype net.vpc { cidr: string }\ncomponent m {\n  type env = string\n}\n";
        assert_eq!(
            kinds(src, &[TYPE_ALIAS, TYPE_DECL, COMPONENT]),
            vec![TYPE_ALIAS, TYPE_DECL, COMPONENT, TYPE_ALIAS]
        );
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn literal_shapes() {
        let src = "p(x) where q(x), x.a, x.b == 1, x in net.vpc, has x.c, \
                   not x.d, v = xs[i], x not in ys, not { r(x) }, e(a: x)\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(
            kinds(
                src,
                &[
                    LIT_ATOM,
                    LIT_TRUTH,
                    LIT_CMP,
                    LIT_IN,
                    LIT_HAS,
                    LIT_NOT,
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
                LIT_NOT,
                LIT_TRUTH,
                LIT_CMP,
                LIT_NOT_IN,
                LIT_NOT_BLOCK,
                LIT_ATOM,
                LIT_ATOM
            ]
        );
        assert_eq!(kinds(src, &[NAMED_ARG]).len(), 1);
    }

    #[test]
    fn a_block_is_the_head_of_the_clause_after_it() {
        let src = "resource net.subnet \"s-${z}\" {\n  \
                   cidr = inet.subnet(vpc.cidr, 4, zone_index[z])\n  zone = z\n} where {\n  \
                   data(\"zone\", z)\n  z != \"x\"\n}\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(kinds(src, &[CLAUSE, ASSIGN]), vec![ASSIGN, ASSIGN, CLAUSE]);
        let clause = parse(src)
            .syntax()
            .descendants()
            .find(|n| n.kind() == CLAUSE)
            .unwrap();
        assert_eq!(clause.parent().unwrap().kind(), RESOURCE);
        // A provider block parses a clause too; the resolver refuses it.
        assert!(errors("provider p { a = 1 } where q(1)\n").is_empty());
    }

    #[test]
    fn a_dot_is_member_access_and_a_slash_division() {
        let src = "p(m.i.n.x, a / b, a/b, t[e].p, f(x))\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(
            kinds(src, &[INDEX, BIN_EXPR]),
            vec![BIN_EXPR, BIN_EXPR, INDEX]
        );
    }

    #[test]
    fn declarations_by_their_columns() {
        let src = "input q(a: string) from csv(\"q.csv\")\ndecl p(a, b: int) mixed\n\
                   output k: int = 1 where p(1, 2)\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(
            kinds(src, &[DECL, INPUT_RELATION, OUTPUT_DECL]),
            vec![INPUT_RELATION, DECL, OUTPUT_DECL]
        );
    }

    /// A file is `edition`, its header, then its body (R-27): a header
    /// statement after the body began is an error that says to move it;
    /// a module's statements are its own.
    #[test]
    fn the_header_comes_before_the_body() {
        let src = "edition 2026\nkey env: string\ninput n: int\n\
                   input p(a) from facts(\"p.facts\")\nuse config\nprovider fake {}\np(1)\n\
                   component m {\n  r(1)\n  input k: int\n}\ninstance m a\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        let src =
            "edition 2026\nprovider fake {}\nkey env: string\nq(1)\ninput p(a) from facts(\"p\")\n";
        let e = parse(src).errors;
        let got: Vec<(&str, bool)> = e
            .iter()
            .map(|e| (e.message.as_str(), e.misplaced))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "`key env` is a header statement: move it above the body's first statement, \
                     line 2",
                    true
                ),
                (
                    "`input p(..)` is a header statement: move it above the body's first \
                     statement, line 2",
                    true
                ),
            ]
        );
    }

    #[test]
    fn a_key_is_an_input() {
        let src = "key env: enum(\"dev\", \"prod\") = \"dev\"\ninput n: int = 1\nkey(\"a\")\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
        assert_eq!(kinds(src, &[INPUT, FACT]), vec![INPUT, INPUT, FACT]);
    }

    #[test]
    fn errors_inside_a_block_recover_at_the_brace() {
        let src = "component m {\n  p(a) where ,\n  q(b)\n}\nr(c)\n";
        assert_eq!(errors(src).len(), 1, "{:?}", errors(src));
    }
}
