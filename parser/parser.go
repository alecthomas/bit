package parser

import (
	"io"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

var parser = participle.MustBuild[Bitfile](
	participle.Lexer(lex),
	participle.Unquote("String", "StringLiteral"),
	participle.Elide("Comment", "WS"),
	participle.UseLookahead(3),
	participle.Union[Entry](&Assignment{}, &Template{}, &Target{}),
	participle.Union[Directive](&Inherit{}, &Dir{}, &Assignment{}, &RefCommand{}, &Command{}),
	participle.Union[Atom](Var{}, Cmd{}, String{}, Path{}),
)

func EBNF() string {
	return parser.String()
}

func Parse(filename string, r io.Reader, options ...participle.ParseOption) (*Bitfile, error) {
	return parser.Parse(filename, r, options...)
}

func ParseString(filename, input string, options ...participle.ParseOption) (*Bitfile, error) {
	return parser.ParseString(filename, input, options...)
}

type Override int

const (
	OverrideReplace Override = iota
	OverridePrepend
	OverrideAppend
	OverrideDelete
)

var _ participle.Parseable = (*Override)(nil)

func (o *Override) GoString() string {
	switch *o {
	case OverrideReplace:
		return "OverrideReplace"
	case OverridePrepend:
		return "OverridePrepend"
	case OverrideAppend:
		return "OverrideAppend"
	case OverrideDelete:
		return "OverrideDelete"
	default:
		return "OverrideUnknown"
	}
}

func (o *Override) Parse(lex *lexer.PeekingLexer) error {
	t := lex.Peek()
	switch t.Value {
	case "^":
		*o = OverridePrepend
	case "+":
		*o = OverrideAppend
	case "-":
		*o = OverrideDelete
	default:
		return participle.NextMatch
	}
	lex.Next()
	return nil
}

type Bitfile struct {
	Entries []Entry `NL* (@@ NL*)* EOF`
}

//sumtype:decl
type Entry interface{ entry() }

type Assignment struct {
	Name     string   `@Ident WS? `
	Override Override `@@?`
	Value    *Block   `"=" WS? @@`
}

func (a *Assignment) entry()     {}
func (a *Assignment) directive() {}

type Target struct {
	Virtual    bool        `(@"virtual" WS)?`
	Inputs     []Atom      `(@@ WS?)+  WS? ":"`
	Outputs    []Atom      `(WS? @@)* WS? NL*`
	Directives []Directive `Indent NL* (@@ NL*)* WS? Dedent`
}

func (t *Target) entry() {}

type Template struct {
	Name       string      `"template" WS @Ident`
	Parameters []Parameter `"(" @@ (WS? "," WS? @@)* WS? ")" WS?`
	Inputs     []Atom      `(@@ WS?)*  WS? ":"`
	Outputs    []Atom      `(WS? @@)* WS? NL*`
	Directives []Directive `Indent NL* (@@ NL*)* WS? Dedent`
}

func (t *Template) entry() {}

//sumtype:decl
type Directive interface{ directive() }

type Inherit struct {
	Target     string      `"<" WS? @Ident`
	Parameters []*Argument `("(" @@ (WS? "," WS? @@)* WS? ")")?`
}

func (i *Inherit) directive() {}

// RefCommand is a command that takes a list of references as its value.
//
// Currently, this includes "inputs" and "outputs".
type RefCommand struct {
	Override Override `@@?`
	Command  string   `@("inputs"|"outputs") WS?`
	Value    []Atom   `( ":" WS? ((Indent (NL* WS* @@)+ Dedent) | (@@ WS?)+) )?`
}

func (i RefCommand) directive() {}

type Command struct {
	Override Override `@@? WS?`
	Command  string   `@Ident WS?`
	Value    *Block   `(":" @@)?`
}

func (i Command) directive() {}

type Argument struct {
	Name  string `@Ident WS? "=" WS?`
	Value string `(@String | @StringLiteral)`
}

type Parameter struct {
	Name  string `@Ident WS? ("=" WS?`
	Value string `(@String | @StringLiteral))?`
}

type Dir struct {
	Target *Block `"dir" WS? ":" @@`
}

func (d *Dir) directive() {}

// A Block is either a single line, or an indented block.
//
// eg.
//
//	inputs: a b
//
// or
//
//	inputs:
//	  a
//	  b
type Block struct {
	Body string `WS? ((Indent NL+ @(WS | ~Dedent)+ Dedent) | @(WS | ~(NL|Dedent))*)`
}

//sumtype:decl
type Atom interface{ atom() }

type Var struct {
	Name string `@Var`
}

func (v Var) atom() {}

type Cmd struct {
	Command string `@Cmd`
}

func (c Cmd) atom() {}

type String struct {
	Value string `@(String | StringLiteral)`
}

func (s String) atom() {}

type Path struct {
	Parts string `@((?!WS) ("/" | "." | "*" | Var | Number | Ident))+`
}

// var _ participle.Parseable = (*Path)(nil)
//
// func (p *Path) Parse(lex *lexer.PeekingLexer) error {
// 	for {
// 		t := lex.Peek()
// 		switch t.Value {
// 		case "/", ".", "*":
// 		}
// 	}
// }

func (p Path) atom() {}
