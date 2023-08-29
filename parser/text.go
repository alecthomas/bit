package parser

import (
	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

var textLexer = lexer.MustSimple([]lexer.SimpleRule{
	{"Cmd", `%\((.|\n)*?\)%`},
	{"Var", `%{[a-zA-Z_][-a-zA-Z0-9_]*}`},
	{"WS", `[ \t\n\r]+`},
	{"Other", `.`},
})
var textParser = participle.MustBuild[Text](
	participle.Lexer(textLexer),
	participle.UseLookahead(3),
	participle.Map(unwrapVar, "Var"),
	participle.Map(unwrapCmd, "Cmd"),
	participle.Union[Fragment](&TextFragment{}, &VarFragment{}, &CmdFragment{}),
)

func ParseTextString(input string) (*Text, error) {
	return textParser.ParseString("", input)
}

// Text is a string with embedded variables and commands.
type Text struct {
	Pos lexer.Position

	Fragments []Fragment `@@+`
}

// Fragment is a fragment of Text.
//
//sumtype:decl
type Fragment interface{ fragment() }

type TextFragment struct {
	Pos lexer.Position

	Text string `@(~(Cmd|Var))+`
}

func (*TextFragment) fragment() {}

type VarFragment struct {
	Pos lexer.Position

	Var string `@Var`
}

func (v *VarFragment) fragment() {}

type CmdFragment struct {
	Pos lexer.Position

	Cmd string `@Cmd`
}

func (v *CmdFragment) fragment() {}
