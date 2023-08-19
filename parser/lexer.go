package parser

import (
	"github.com/alecthomas/participle/v2/lexer"

	"github.com/alecthomas/bit/parser/lexer/continuation"
	"github.com/alecthomas/bit/parser/lexer/indenter"
)

var lex = continuation.New(indenter.New(lexer.MustStateful(lexer.Rules{
	"Root": {
		{"Continuation", `[ \t]*\\\n[ \t]*`, nil},
		{"NL", `[\n][ \t]*`, nil},
		{"Comment", "#[^\n]*", nil},
		{"String", `"(\\.|[^"])*"`, nil}, // This will need to be a LOT smarter do deal with Bash strings.
		{"StringLiteral", `'[^']*'`, nil},
		{"MultilineString", `'''`, lexer.Push("MultilineString")},
		{"Ident", `[a-zA-Z_][-a-zA-Z0-9_]*`, nil},
		{"Cmd", `%\(.*?\)%`, nil},
		{"Var", `%{[a-zA-Z_][-a-zA-Z0-9_]*}`, nil},
		{"Number", `[0-9]+`, nil},
		{"WS", `[ \t]+`, nil},
		{"Other", `.`, nil},
	},
	"MultilineString": {
		{"MultilineStringEnd", `'''`, lexer.Pop()},
		{"MultilineStringContent", `'|([^']*)`, nil},
	},
})))
