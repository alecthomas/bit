#include "tree_sitter/parser.h"
#include <wctype.h>
#include <string.h>

// Heredoc body is tokenised piecewise so `#{...}` interpolations inside
// it are exposed to the grammar (and therefore to highlight queries)
// instead of being swallowed into one opaque blob.
//
// External token order matters: tree-sitter tries earlier symbols first
// when several are `valid_symbols`. HEREDOC_END is listed before
// HEREDOC_CONTENT so a terminator line wins over more body content.
enum TokenType {
    HEREDOC_LABEL_EOL,  // consumes the label (right after `<<` / `<<-`) plus its trailing newline.
    HEREDOC_END,        // consumes the terminator line (optional leading whitespace + label + EOL/EOF).
    HEREDOC_CONTENT,    // consumes literal body text up to the next `#{`, the terminator line, or EOF.
};

#define MAX_LABEL 128

// Persistent scanner state across `scan` calls. Serialised so incremental
// reparse after edits preserves the heredoc label we are currently inside.
typedef struct {
    unsigned char label_len;
    char label[MAX_LABEL];
} Scanner;

void *tree_sitter_bit_external_scanner_create(void) {
    Scanner *s = (Scanner *)malloc(sizeof(Scanner));
    if (s) {
        s->label_len = 0;
    }
    return s;
}

void tree_sitter_bit_external_scanner_destroy(void *payload) {
    free(payload);
}

unsigned tree_sitter_bit_external_scanner_serialize(void *payload, char *buffer) {
    Scanner *s = (Scanner *)payload;
    if (s->label_len == 0) {
        return 0;
    }
    buffer[0] = (char)s->label_len;
    memcpy(buffer + 1, s->label, s->label_len);
    return 1 + s->label_len;
}

void tree_sitter_bit_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
    Scanner *s = (Scanner *)payload;
    s->label_len = 0;
    if (length == 0) {
        return;
    }
    unsigned char n = (unsigned char)buffer[0];
    if (n == 0 || (unsigned)(1 + n) > length || n > MAX_LABEL) {
        return;
    }
    s->label_len = n;
    memcpy(s->label, buffer + 1, n);
}

static inline void advance(TSLexer *lexer) { lexer->advance(lexer, false); }

static bool is_label_cont(int32_t c) {
    return (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') ||
           (c >= 'a' && c <= 'z') || c == '_';
}

// Try to match the terminator label at the current position. Returns true
// if the lookahead is "[ \t]* <label> (EOL|EOF)". On both true and false
// paths characters may have been advanced past; the caller is responsible
// for calling `mark_end` to commit them (on false) or for breaking out
// before any mark_end (on true) so the terminator stays unconsumed for
// HEREDOC_END.
static bool match_terminator(Scanner *s, TSLexer *lexer) {
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
        advance(lexer);
    }
    size_t matched = 0;
    while (matched < s->label_len && lexer->lookahead == (int32_t)(unsigned char)s->label[matched]) {
        advance(lexer);
        matched++;
    }
    if (matched != s->label_len) {
        return false;
    }
    return lexer->eof(lexer) || lexer->lookahead == '\n' || lexer->lookahead == '\r';
}

// Consume the rest of the terminator line (trailing CR/LF) after
// `match_terminator` returned true. Used by HEREDOC_END so the heredoc
// node spans the whole closing line.
static void consume_terminator_eol(TSLexer *lexer) {
    if (lexer->lookahead == '\r') {
        advance(lexer);
    }
    if (lexer->lookahead == '\n') {
        advance(lexer);
    }
}

// HEREDOC_LABEL_EOL: read label immediately after `<<` or `<<-`,
// stopping AT (not past) the trailing newline. The newline is left
// for the next HEREDOC_CONTENT token so that heredoc_body's start
// byte is one before the first body character. This matters for
// Zed's highlighter: when `(heredoc_body) @string` and a child
// `(interpolation "#{")` capture share the same start byte, Zed
// pushes the wider capture LAST onto its highlight stack, making
// @string override the child @punctuation.special / @variable.
// Giving heredoc_body a leading newline of its own breaks the tie,
// mirroring how (string) @string starts at the opening quote
// before any child interpolation can begin.
static bool scan_label(Scanner *s, TSLexer *lexer) {
    size_t len = 0;
    while (is_label_cont(lexer->lookahead) && len < MAX_LABEL) {
        s->label[len++] = (char)lexer->lookahead;
        advance(lexer);
    }
    if (len == 0) {
        return false;
    }
    if (lexer->lookahead != '\n') {
        return false;
    }
    s->label_len = (unsigned char)len;
    lexer->result_symbol = HEREDOC_LABEL_EOL;
    return true;
}

// HEREDOC_END: optional leading whitespace + label + EOL/EOF. Clears
// scanner state so a subsequent heredoc starts clean. Also accepts an
// unterminated heredoc at bare EOF as an implicit terminator (mirrors
// the previous scanner's error-recovery behaviour).
static bool scan_end(Scanner *s, TSLexer *lexer) {
    if (s->label_len == 0) {
        return false;
    }
    if (lexer->eof(lexer)) {
        s->label_len = 0;
        lexer->result_symbol = HEREDOC_END;
        return true;
    }
    if (!match_terminator(s, lexer)) {
        return false;
    }
    consume_terminator_eol(lexer);
    s->label_len = 0;
    lexer->result_symbol = HEREDOC_END;
    return true;
}

// HEREDOC_CONTENT: consume literal body characters until `#{`, the
// terminator line, or EOF. Must consume at least one character;
// otherwise returns false so the grammar can match `interpolation` or
// HEREDOC_END at the same position.
//
// Line-start detection uses `lexer->get_column`; at column 0 we peek
// for the terminator only if the first character could plausibly match
// (whitespace or the first char of the label), avoiding spurious zero-
// length tokens that would otherwise infinite-loop.
static bool scan_content(Scanner *s, TSLexer *lexer) {
    if (s->label_len == 0) {
        return false;
    }

    bool consumed = false;
    lexer->mark_end(lexer);

    while (!lexer->eof(lexer)) {
        if (lexer->get_column(lexer) == 0) {
            int32_t c = lexer->lookahead;
            // Cheap pre-check: only the terminator (or its leading
            // whitespace) can start this way, so save the costlier
            // multi-char match for those cases.
            if (c == ' ' || c == '\t' || c == (int32_t)(unsigned char)s->label[0]) {
                if (match_terminator(s, lexer)) {
                    // Stop without consuming; HEREDOC_END picks it up.
                    break;
                }
                // Not the terminator. match_terminator advanced past
                // some whitespace and/or label-prefix chars; commit
                // them as literal content.
                lexer->mark_end(lexer);
                consumed = true;
                continue;
            }
        }

        int32_t c = lexer->lookahead;

        if (c == '#') {
            advance(lexer);
            if (lexer->lookahead == '{') {
                // Stop before the `#` so `interpolation` can match it.
                // mark_end was last called before this advance, so the
                // `#` is rewound on return.
                break;
            }
            // Lone `#`: literal heredoc content.
            lexer->mark_end(lexer);
            consumed = true;
            continue;
        }

        advance(lexer);
        lexer->mark_end(lexer);
        consumed = true;
    }

    if (!consumed) {
        return false;
    }
    lexer->result_symbol = HEREDOC_CONTENT;
    return true;
}

bool tree_sitter_bit_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
    Scanner *s = (Scanner *)payload;

    if (valid_symbols[HEREDOC_LABEL_EOL]) {
        if (scan_label(s, lexer)) {
            return true;
        }
    }
    if (valid_symbols[HEREDOC_END]) {
        if (scan_end(s, lexer)) {
            return true;
        }
    }
    if (valid_symbols[HEREDOC_CONTENT]) {
        if (scan_content(s, lexer)) {
            return true;
        }
    }
    return false;
}
