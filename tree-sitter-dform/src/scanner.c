// The two tokens a grammar cannot say: a newline that ends a statement, and
// a string's literal text. A newline is a token only where the grammar can
// take one (outside brackets, after something complete); everywhere else it
// is whitespace, as in the compiler's parser
// (crates/dform-core/src/syntax/parser.rs). Nothing continues a line.

#include "tree_sitter/parser.h"

enum TokenType {
  NEWLINE,
  STRING_CONTENT,
  ERROR_SENTINEL,
};

void *tree_sitter_dform_external_scanner_create(void) { return NULL; }
void tree_sitter_dform_external_scanner_destroy(void *payload) {}
unsigned tree_sitter_dform_external_scanner_serialize(void *payload, char *buffer) { return 0; }
void tree_sitter_dform_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {}

bool tree_sitter_dform_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
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

  if (valid_symbols[NEWLINE]) {
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t' || lexer->lookahead == '\r' ||
           lexer->lookahead == '\f') {
      lexer->advance(lexer, true);
    }
    if (lexer->lookahead != '\n') return false;
    lexer->advance(lexer, false);
    lexer->result_symbol = NEWLINE;
    return true;
  }
  return false;
}
