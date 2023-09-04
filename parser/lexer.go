package parser

import (
	"strings"

	"github.com/alecthomas/participle/v2/lexer"

	"github.com/alecthomas/bit/parser/lexer/continuation"
	"github.com/alecthomas/bit/parser/lexer/indenter"
)

var baseLexer = lexer.MustStateful(lexer.Rules{
	"Root": {
		{"Continuation", `[ \t]*\\\n[ \t]*`, nil},
		{"NL", `[\n][ \t]*`, nil},
		{"Comment", "#[^\n]*\n?", nil},
		{"String", `"(\\.|[^"])*"`, nil}, // This will need to be a LOT smarter do deal with Bash strings.
		{"StringLiteral", `'[^']*'`, nil},
		{"MultilineString", `'''`, lexer.Push("MultilineString")},
		{"Ident", `[a-zA-Z_][-a-zA-Z0-9_]*`, nil},
		{"Cmd", `%\((.|\n)*?\)%`, nil},
		{"Var", `%{[0-9a-zA-Z_][-a-zA-Z0-9_]*}`, nil},
		{"Number", `[0-9]+`, nil},
		{"WS", `[ \t]+`, nil},
		{"Other", `.`, nil},
	},
	"MultilineString": {
		{"MultilineStringEnd", `'''`, lexer.Pop()},
		{"MultilineStringContent", `'|([^']*)`, nil},
	},
})
var lex = continuation.New(indenter.New(baseLexer))

func cleanComment(token lexer.Token) (lexer.Token, error) {
	token.Value = strings.TrimSpace(strings.TrimPrefix(token.Value, "#"))
	return token, nil
}

func unquoteMultilineString(t lexer.Token) (lexer.Token, error) { //nolint:unparam
	t.Value = t.Value[3 : len(t.Value)-3]
	return t, nil
}

func unquoteStringLiteral(t lexer.Token) (lexer.Token, error) { //nolint:unparam
	t.Value = t.Value[1 : len(t.Value)-1]
	return t, nil
}

func unwrapCmd(t lexer.Token) (lexer.Token, error) {
	t.Value = t.Value[2 : len(t.Value)-2]
	return t, nil
}

func unwrapVar(t lexer.Token) (lexer.Token, error) {
	t.Value = t.Value[2 : len(t.Value)-1]
	return t, nil
}
