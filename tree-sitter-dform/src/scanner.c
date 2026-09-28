// The two tokens a grammar cannot say: a newline that ends a statement, and
// a string's literal text. A newline is a token only where the grammar can
// take one (outside brackets, after something complete); everywhere else it
// is whitespace, as in the compiler's parser (src/syntax/parser.rs), which
// also lets a line that starts with `,` or a rank continue the one before.

#include "tree_sitter/parser.h"

#include <wctype.h>

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
    bool any = false;
    for (;;) {
      switch (lexer->lookahead) {
        case '"':
        case '\\':
        case '{':
        case '}':
          lexer->result_symbol = STRING_CONTENT;
          return any;
        case 0:
          if (lexer->eof(lexer)) {
            lexer->result_symbol = STRING_CONTENT;
            return any;
          }
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
    lexer->mark_end(lexer);
    lexer->result_symbol = NEWLINE;
    // Look past blank lines and comments: a line that starts with `,` or a
    // rank continues the one before, as the compiler reads it.
    for (;;) {
      while (iswspace(lexer->lookahead)) lexer->advance(lexer, false);
      if (lexer->lookahead == '#') {
        while (lexer->lookahead != '\n' && !lexer->eof(lexer)) lexer->advance(lexer, false);
      } else if (lexer->lookahead == '/') {
        lexer->advance(lexer, false);
        if (lexer->lookahead != '/') return true;
        while (lexer->lookahead != '\n' && !lexer->eof(lexer)) lexer->advance(lexer, false);
      } else {
        break;
      }
      if (lexer->eof(lexer)) return true;
    }
    return lexer->lookahead != ',' && lexer->lookahead != '@';
  }
  return false;
}
