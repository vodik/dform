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
    PATH,
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
    /// `:-`: lexed only so an old rule is told it is spelled `if`.
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
    // Keywords.
    EDITION_KW,
    PROVIDER_KW,
    STACK_KW,
    IMPORT_KW,
    INPUT_KW,
    OUTPUT_KW,
    EXPORT_KW,
    CONTRIBUTES_KW,
    MODULE_KW,
    INSTANCE_KW,
    POLICY_KW,
    APPLY_KW,
    RESOURCE_KW,
    SETTINGS_KW,
    SCENARIO_KW,
    EXTERN_KW,
    TYPE_KW,
    DECL_KW,
    WHEN_KW,
    NOT_KW,
    IN_KW,
    EXISTS_KW,
    TRUE_KW,
    FALSE_KW,
    NULL_KW,
    PERSIST_KW,
    WHERE_KW,
    IF_KW,
    FOR_KW,
    LET_KW,
    HAS_KW,
    SOME_KW,
    WITH_KW,
    DENY_KW,
    WARN_KW,
    CONSTRAINT_KW,
    /// A character no token starts with.
    ERROR_TOKEN,

    // Nodes.
    SOURCE_FILE,
    /// Tokens skipped by error recovery.
    ERROR,
    EDITION,
    IMPORT,
    PROVIDER,
    STACK,
    /// `key = term` inside a provider or stack block.
    KV,
    INPUT,
    /// `input relation p/N from source(...)`: a relation fed from outside.
    INPUT_RELATION,
    OUTPUT_DECL,
    EXPORT,
    CONTRIBUTES,
    EXTERN,
    BIND_ARG,
    TYPE_DECL,
    ATTR_DECL,
    TYPE_EXPR,
    DECL,
    MODULE,
    INSTANCE,
    POLICY,
    APPLY,
    SCENARIO,
    WHEN,
    RESOURCE,
    SETTINGS,
    /// `{ clause* assign* }` of a resource, settings or instance.
    BLOCK,
    ASSIGN,
    BLOCK_PATH,
    /// `{ stmt* }` of a module, policy, scenario, `when` or `for`.
    STMT_BLOCK,
    RULE,
    FACT,
    BODY,
    WHERE_CLAUSE,
    LIT_ATOM,
    LIT_NOT,
    LIT_CMP,
    LIT_IN,
    LIT_NOT_IN,
    ATOM,
    RECORD_ATOM,
    RECORD_FIELD,
    ARG_LIST,
    // Terms.
    LITERAL,
    PATH_LIT,
    CALL,
    LIST,
    OBJECT,
    OBJECT_FIELD,
    COMPREHENSION,
    PAREN,
    BIN_EXPR,
    UNARY_EXPR,
    // Nodes of the proposal G surface.
    /// `name (.seg | [terms] | /name)*`, parsed unresolved.
    CHAIN,
    /// `[t, ...]` after a chain.
    INDEX,
    /// `for body` or `if body` at the top of a block.
    CLAUSE,
    /// `let a = chain`.
    LET,
    /// `with k = t` in a scenario.
    WITH,
    /// `for body { stmts }`.
    FOR_STMT,
    /// `deny|warn|constraint "msg" [object] [if body]`.
    CHECK,
    /// `chain.path (=|+=) term [rank] [if body]`.
    CONTRIBUTION,
    /// `name = term [if body]`.
    VALUE_RULE,
    /// A chain alone as a literal: a truth test.
    LIT_TRUTH,
    LIT_EXISTS,
    LIT_HAS,
    /// `some b [, b] in term`.
    LIT_SOME,
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

    pub fn is_keyword(self) -> bool {
        (EDITION_KW as u16..=CONSTRAINT_KW as u16).contains(&(self as u16))
    }

    /// How a token kind is named in "expected ..." diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            WHITESPACE => "whitespace",
            COMMENT => "a comment",
            IDENT => "a name",
            PATH => "a keypath",
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
