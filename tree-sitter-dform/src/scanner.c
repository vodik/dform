// The tokens a grammar cannot say: a newline that ends a statement, a
// string's literal text, and the line breaks that continue a `where`
// clause. A newline is a token only where the grammar can take one
// (outside brackets, after something complete); everywhere else it is
// whitespace, as in the compiler's parser
// (crates/dform-core/src/syntax/parser.rs). A clause continues on a line
// indented deeper than its statement: after a comma (`CLAUSE_LINE`), and
// at a `where` that starts the line (`HANG`). The statement's indentation
// is the scanner's state: that of the line each newline it emits leads
// to, blank lines and comments aside.

#include "tree_sitter/alloc.h"
#include "tree_sitter/parser.h"

#include <string.h>

enum TokenType {
  NEWLINE,
  STRING_CONTENT,
  ERROR_SENTINEL,
  CLAUSE_LINE,
  HANG,
};

typedef struct {
  uint32_t indent;
} Scanner;

void *tree_sitter_dform_external_scanner_create(void) {
  Scanner *s = ts_malloc(sizeof(Scanner));
  s->indent = 0;
  return s;
}

void tree_sitter_dform_external_scanner_destroy(void *payload) { ts_free(payload); }

unsigned tree_sitter_dform_external_scanner_serialize(void *payload, char *buffer) {
  memcpy(buffer, payload, sizeof(Scanner));
  return sizeof(Scanner);
}

void tree_sitter_dform_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
  Scanner *s = payload;
  if (length == sizeof(Scanner)) {
    memcpy(s, buffer, sizeof(Scanner));
  } else {
    s->indent = 0;
  }
}

static bool is_word(int32_t c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_';
}

// Past blank lines and comment lines to the next line with code: its
// indentation, or -1 at the end of the file.
static int64_t next_code_line(TSLexer *lexer) {
  for (;;) {
    int64_t col = 0;
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
      col++;
      lexer->advance(lexer, false);
    }
    if (lexer->eof(lexer)) return -1;
    if (lexer->lookahead == '#') {
      while (lexer->lookahead != '\n' && !lexer->eof(lexer)) lexer->advance(lexer, false);
    }
    if (lexer->lookahead == '\n' || lexer->lookahead == '\r' || lexer->lookahead == '\f') {
      lexer->advance(lexer, false);
      continue;
    }
    if (lexer->eof(lexer)) return -1;
    return col;
  }
}

// At the word `where` and nothing after it that continues a name.
static bool at_where(TSLexer *lexer) {
  for (const char *w = "where"; *w; w++) {
    if (lexer->lookahead != *w) return false;
    lexer->advance(lexer, false);
  }
  return !is_word(lexer->lookahead);
}

bool tree_sitter_dform_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
  Scanner *s = payload;
  // Error recovery marks every token valid; let the internal lexer have it.
  if (valid_symbols[ERROR_SENTINEL]) return false;

  if (valid_symbols[STRING_CONTENT]) {
    // Up to a quote, an escape, a hole `${` or an escaped `$${`.
    bool any = false;
    lexer->result_symbol = STRING_CONTENT;
    for (;;) {
      lexer->mark_end(lexer);
      switch (lexer->lookahead) {
        case '"':
        case '\\':
          return any;
        case '$':
          lexer->advance(lexer, false);
          if (lexer->lookahead == '{') return any;
          if (lexer->lookahead == '$') {
            lexer->advance(lexer, false);
            if (lexer->lookahead == '{') return any;
          }
          any = true;
          continue;
        case 0:
          if (lexer->eof(lexer)) return any;
          break;
      }
      lexer->advance(lexer, false);
      any = true;
    }
  }

  if (valid_symbols[NEWLINE] || valid_symbols[CLAUSE_LINE] || valid_symbols[HANG]) {
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t' || lexer->lookahead == '\r' ||
           lexer->lookahead == '\f') {
      lexer->advance(lexer, true);
    }
    if (lexer->lookahead != '\n') return false;
    lexer->advance(lexer, false);
    lexer->mark_end(lexer);
    int64_t col = next_code_line(lexer);
    bool deeper = col > (int64_t)s->indent;
    if (valid_symbols[HANG] && deeper && at_where(lexer)) {
      lexer->result_symbol = HANG;
      return true;
    }
    if (valid_symbols[CLAUSE_LINE] && deeper) {
      lexer->result_symbol = CLAUSE_LINE;
      return true;
    }
    if (!valid_symbols[NEWLINE]) return false;
    if (col >= 0) s->indent = (uint32_t)col;
    lexer->result_symbol = NEWLINE;
    return true;
  }
  return false;
}
