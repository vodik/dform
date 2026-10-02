//! Token and node kinds of the lossless syntax tree (docs/grammar.md).

/// Every token the lexer produces and every node the parser builds. Tokens
/// come first; `is_trivia` and `is_keyword` rely on the order.
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    // Trivia.
    WHITESPACE = 0,
    COMMENT,
    // Names and literals.
    IDENT,
    STRING,
    INT,
    RANK,
    // Punctuation.
    L_PAREN,
    R_PAREN,
    L_BRACE,
    R_BRACE,
    L_BRACKET,
    R_BRACKET,
    COMMA,
    DOT,
    COLON,
    /// `:-`: lexed only so an old rule is told it is spelled `where`.
    NECK,
    EQ,
    EQ2,
    PLUS_EQ,
    NEQ,
    LT,
    LE,
    GT,
    GE,
    PLUS,
    MINUS,
    STAR,
    SLASH,
    PERCENT,
    PIPE,
    // Keywords: the statement keywords (a statement's first token), then
    // the body words, the clause word and the literals.
    EDITION_KW,
    IMPORT_KW,
    PROVIDER_KW,
    STACK_KW,
    TYPE_KW,
    DECL_KW,
    EXTERN_KW,
    INPUT_KW,
    OUTPUT_KW,
    LET_KW,
    SET_KW,
    EXPORT_KW,
    CONTRIBUTES_KW,
    MODULE_KW,
    INSTANCE_KW,
    POLICY_KW,
    USE_KW,
    SCENARIO_KW,
    RESOURCE_KW,
    SETTINGS_KW,
    DENY_KW,
    WARN_KW,
    NOT_KW,
    IN_KW,
    HAS_KW,
    /// `where`: the clause word, after the head (R-1).
    WHERE_KW,
    /// `if`: reserved, so that the clause word of an earlier surface is an
    /// error that prints the `where` form.
    IF_KW,
    TRUE_KW,
    FALSE_KW,
    /// A character no token starts with.
    ERROR_TOKEN,
    /// The end of a line where a newline ends what is being parsed: the
    /// parser's lookahead only, never in the tree.
    NEWLINE,

    // Nodes.
    SOURCE_FILE,
    /// Tokens skipped by error recovery.
    ERROR,
    EDITION,
    IMPORT,
    PROVIDER,
    STACK,
    /// `input k: T [= t] [check B]`.
    INPUT,
    /// `input p(cols) from FORMAT(SOURCE)`: a relation fed from outside.
    INPUT_RELATION,
    /// `output k [: T] = t [where B]`.
    OUTPUT_DECL,
    EXPORT,
    CONTRIBUTES,
    EXTERN,
    /// An extern's `+name: T`, a column `name [: T]`.
    BIND_ARG,
    TYPE_DECL,
    ATTR_DECL,
    TYPE_EXPR,
    DECL,
    MODULE,
    INSTANCE,
    POLICY,
    USE,
    SCENARIO,
    RESOURCE,
    SETTINGS,
    /// `{ entry* }` of a resource, settings, instance, provider or stack.
    BLOCK,
    /// `path (=|+=) term [rank]` in a block.
    ASSIGN,
    BLOCK_PATH,
    /// `{ stmt* }` of a module, policy or scenario.
    STMT_BLOCK,
    RULE,
    FACT,
    BODY,
    /// `check body` after an input's or a type attribute's type: a
    /// refinement.
    REFINEMENT,
    LIT_ATOM,
    LIT_NOT,
    LIT_CMP,
    LIT_IN,
    LIT_NOT_IN,
    ARG_LIST,
    /// `name: term` in an argument list.
    NAMED_ARG,
    // Terms.
    LITERAL,
    CALL,
    LIST,
    OBJECT,
    OBJECT_FIELD,
    COMPREHENSION,
    PAREN,
    BIN_EXPR,
    UNARY_EXPR,
    /// `name (.seg | [terms])*`, parsed unresolved.
    CHAIN,
    /// `[t, ...]` after a chain.
    INDEX,
    /// `where body` after a block: its clause.
    CLAUSE,
    /// `let k = t [where B]`.
    LET,
    /// `set chain (=|+=) t [rank] [where B]`: a contribution.
    SET,
    /// `deny|warn "msg" [object] [where body]`.
    CHECK,
    /// A chain alone as a literal: a truth test.
    LIT_TRUTH,
    LIT_HAS,
    /// `not { body }`.
    LIT_NOT_BLOCK,
    /// `type NAME = TYPE`: a type alias.
    TYPE_ALIAS,
    __LAST,
}

use SyntaxKind::*;

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, WHITESPACE | COMMENT)
    }

    /// A keyword a statement starts with (H section 4).
    pub fn is_stmt_keyword(self) -> bool {
        (EDITION_KW as u16..=WARN_KW as u16).contains(&(self as u16))
    }

    pub fn is_keyword(self) -> bool {
        (EDITION_KW as u16..=FALSE_KW as u16).contains(&(self as u16))
    }

    /// How a token kind is named in "expected ..." diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            WHITESPACE => "whitespace",
            COMMENT => "a comment",
            IDENT => "a name",
            STRING => "a string",
            INT => "an integer",
            RANK => "a rank",
            L_PAREN => "`(`",
            R_PAREN => "`)`",
            L_BRACE => "`{`",
            R_BRACE => "`}`",
            L_BRACKET => "`[`",
            R_BRACKET => "`]`",
            COMMA => "`,`",
            DOT => "`.`",
            COLON => "`:`",
            NECK => "`:-`",
            EQ => "`=`",
            EQ2 => "`==`",
            PLUS_EQ => "`+=`",
            NEQ => "`!=`",
            LT => "`<`",
            LE => "`<=`",
            GT => "`>`",
            GE => "`>=`",
            PLUS => "`+`",
            MINUS => "`-`",
            STAR => "`*`",
            SLASH => "`/`",
            PERCENT => "`%`",
            PIPE => "`|`",
            ERROR_TOKEN => "an unknown character",
            NEWLINE => "the end of the line",
            k if k.is_keyword() => "a keyword",
            _ => "a node",
        }
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(k: SyntaxKind) -> Self {
        Self(k as u16)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {}

impl rowan::Language for Lang {
    type Kind = SyntaxKind;
    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        assert!(raw.0 < __LAST as u16);
        // SAFETY: SyntaxKind is repr(u16) and raw.0 is in range.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }
    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        kind.into()
    }
}

pub type SyntaxNode = rowan::SyntaxNode<Lang>;
pub type SyntaxToken = rowan::SyntaxToken<Lang>;
pub type SyntaxElement = rowan::SyntaxElement<Lang>;
