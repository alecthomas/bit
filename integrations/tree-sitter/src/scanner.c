#include "tree_sitter/parser.h"
#include <wctype.h>
#include <string.h>

enum TokenType {
    HEREDOC_BODY,
};

void *tree_sitter_bit_external_scanner_create(void) { return NULL; }
void tree_sitter_bit_external_scanner_destroy(void *payload) { (void)payload; }
unsigned tree_sitter_bit_external_scanner_serialize(void *payload, char *buffer) {
    (void)payload; (void)buffer; return 0;
}
void tree_sitter_bit_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
    (void)payload; (void)buffer; (void)length;
}

static inline void advance(TSLexer *lexer) { lexer->advance(lexer, false); }

static bool is_label_cont(int32_t c) {
    return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') ||
           (c >= 'a' && c <= 'z') || c == '_';
}

// Scans the label + body + terminator of a heredoc, starting immediately
// after the `<<` or `<<-` marker consumed by the grammar.
bool tree_sitter_bit_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
    (void)payload;

    if (!valid_symbols[HEREDOC_BODY]) {
        return false;
    }

    // Read label.
    char label[128];
    size_t label_len = 0;
    while (is_label_cont(lexer->lookahead) && label_len < sizeof(label) - 1) {
        label[label_len++] = (char)lexer->lookahead;
        advance(lexer);
    }
    if (label_len == 0) return false;
    label[label_len] = '\0';

    // Require newline after label.
    if (lexer->lookahead != '\n') return false;
    advance(lexer);

    // Scan lines until the terminator.
    while (!lexer->eof(lexer)) {
        // Leading whitespace is allowed before the terminator label.
        while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
            advance(lexer);
        }

        size_t matched = 0;
        while (matched < label_len && lexer->lookahead == (int32_t)(unsigned char)label[matched]) {
            advance(lexer);
            matched++;
        }

        if (matched == label_len &&
            (lexer->eof(lexer) || lexer->lookahead == '\n' || lexer->lookahead == '\r')) {
            if (lexer->lookahead == '\r') advance(lexer);
            if (lexer->lookahead == '\n') advance(lexer);
            lexer->result_symbol = HEREDOC_BODY;
            return true;
        }

        // Not the terminator — consume the rest of this line as body.
        while (lexer->lookahead != '\n' && !lexer->eof(lexer)) {
            advance(lexer);
        }
        if (lexer->lookahead == '\n') {
            advance(lexer);
        }
    }

    // Accept unterminated heredocs at EOF for better error recovery.
    lexer->result_symbol = HEREDOC_BODY;
    return true;
}
