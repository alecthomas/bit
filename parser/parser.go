package parser

import (
	"io"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

var parser = participle.MustBuild[Bitfile](
	participle.Lexer(lex),
	participle.Unquote("String"),
	participle.Map(unquoteStringLiteral, "StringLiteral"),
	participle.Map(unquoteMultilineString, "MultilineString"),
	participle.Elide("Comment", "WS"),
	participle.UseLookahead(3),
	participle.Union[Entry](&Template{}, &VirtualTarget{}, &Assignment{}, &Target{}),
	participle.Union[Directive](&Inherit{}, &Dir{}, &Assignment{}, &RefCommand{}, &Command{}),
	participle.Union[Atom](Var{}, Cmd{}, String{}, Path{}),
)

//sumtype:decl
type Node interface{ children() []Node }

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
type Entry interface {
	Node
	entry()
}

// A Buildable entry is something that can be built.
type Buildable interface {
	Entry
	buildable()
}

type Assignment struct {
	Name     string   `@Ident`
	Override Override `@@?`
	Value    *Block   `"=" @@`
}

func (a *Assignment) entry()     {}
func (a *Assignment) directive() {}

type Target struct {
	Outputs    []Atom      `(@@ WS?)* ":"`
	Inputs     []Atom      `(WS? @@)* NL*`
	Directives []Directive `Indent NL* (@@ NL*)* Dedent`
}

func (*Target) entry()     {}
func (*Target) buildable() {}

type VirtualTarget struct {
	Name       string      `"virtual" @Ident ":"`
	Inputs     []Atom      `(WS? @@)* NL*`
	Directives []Directive `Indent NL* (@@ NL*)* Dedent`
}

func (*VirtualTarget) entry()     {}
func (*VirtualTarget) buildable() {}

type Template struct {
	Name       string      `"template" @Ident`
	Parameters []Parameter `"(" @@ ("," @@)* ")"`
	Outputs    []Atom      `@@* ":"`
	Inputs     []Atom      `@@* NL*`
	Directives []Directive `Indent NL* (@@ NL*)* Dedent`
}

func (*Template) entry() {}

//sumtype:decl
type Directive interface {
	Node
	directive()
}

type Inherit struct {
	Target     string      `"<" @Ident`
	Parameters []*Argument `("(" @@ ("," @@)* ")")?`
}

func (i *Inherit) directive() {}

// RefCommand is a command that takes a list of references as its value.
//
// Currently, this includes "inputs" and "outputs".
type RefCommand struct {
	Override Override `@@?`
	Command  string   `@("inputs"|"outputs")`
	Value    []Atom   `( ":" WS? ((Indent (NL* @@)+ Dedent) | @@+) )?`
}

func (r *RefCommand) directive() {}

type Command struct {
	Override Override `@@?`
	Command  string   `@Ident`
	Value    *Block   `(":" @@)?`
}

func (c *Command) directive() {}

type Argument struct {
	Name  string `@Ident "="`
	Value String `@@`
}

type Parameter struct {
	Name  string `@Ident ("="`
	Value string `        (@String | @StringLiteral))?`
}

type Dir struct {
	Target *Block `"dir" ":" @@`
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
type Atom interface {
	Node
	atom()
}

type Var struct {
	Name string `@Var`
}

func (v Var) atom() {}

type Cmd struct {
	Command string `@Cmd`
}

func (c Cmd) atom() {}

type String struct {
	Value string `@(String | StringLiteral | MultilineString)`
}

func (s String) atom() {}

type Path struct {
	// This is a bit hairy because we need to explicitly match WS
	// to "un"-elide it, but we don't want to capture it.
	Parts string `@((?!WS) ("/" | "." | "*" | Var | Number | Ident | Cmd))+`
}

func (p Path) atom() {}
