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
    VAR,
    QNAME,
    FIELD,
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
    // Keywords (E §6), in the order of the list there.
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
    COLLECT_SET_KW,
    COLLECT_LIST_KW,
    COLLECT_ORDERED_KW,
    COUNT_KW,
    SUM_KW,
    MIN_KW,
    MAX_KW,
    LUB_RANKED_KW,
    ALLOCATE_KW,
    TRUE_KW,
    FALSE_KW,
    NULL_KW,
    SECRET_KW,
    PERSIST_KW,
    WHERE_KW,
    MOVED_KW,
    ADOPT_KW,
    LIFECYCLE_KW,
    IGNORE_CHANGES_KW,
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
    /// `{ assign* }` of a resource, settings or instance.
    BLOCK,
    ASSIGN,
    BLOCK_PATH,
    /// `{ (stmt ".")* }` of a module, policy, scenario or when.
    STMT_BLOCK,
    RULE,
    FACT,
    BODY,
    WHERE_CLAUSE,
    LIT_ATOM,
    LIT_NOT,
    LIT_NOT_EXISTS,
    LIT_CMP,
    LIT_IN,
    LIT_NOT_IN,
    ATOM,
    RECORD_ATOM,
    RECORD_FIELD,
    ARG_LIST,
    // Terms.
    LITERAL,
    NAME_REF,
    VAR_REF,
    PATH_LIT,
    FIELD_ACCESS,
    ADDR,
    /// `ident.Var`: a qualified name with a variable segment.
    QNAME_VAR,
    CALL,
    LIST,
    OBJECT,
    OBJECT_FIELD,
    COMPREHENSION,
    PAREN,
    BIN_EXPR,
    UNARY_EXPR,
    __LAST,
}

use SyntaxKind::*;

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, WHITESPACE | COMMENT)
    }

    pub fn is_keyword(self) -> bool {
        (EDITION_KW as u16..=IGNORE_CHANGES_KW as u16).contains(&(self as u16))
    }

    /// A token that can stand where a plain name is expected: a key in a
    /// block path, an object or a record, or a symbol in term position.
    /// Keywords are token kinds, but outside their statement they are names.
    pub fn is_name(self) -> bool {
        self == IDENT || (self.is_keyword() && !matches!(self, TRUE_KW | FALSE_KW | NULL_KW))
    }

    /// How a token kind is named in "expected ..." diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            WHITESPACE => "whitespace",
            COMMENT => "a comment",
            IDENT => "an identifier",
            VAR => "a variable",
            QNAME => "a qualified name",
            FIELD => "a field access",
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
