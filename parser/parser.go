package parser

import (
	"io"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

var parserOptions = []participle.Option{}
var parser = participle.MustBuild[Bitfile](
	participle.Lexer(lex),
	participle.Elide("Comment", "WS"),
	participle.UseLookahead(3),
	participle.Union[Entry](&Template{}, &VirtualTarget{}, &Assignment{}, &Target{}),
	participle.Union[Directive](&Inherit{}, &Dir{}, &Assignment{}, &RefCommand{}, &Command{}),
)

// Node is a node in the AST.
//
//sumtype:decl
type Node interface {
	Position() lexer.Position
	children() []Node
}

func EBNF() string {
	return parser.String()
}

func Parse(filename string, r io.Reader) (*Bitfile, error) {
	return parser.Parse(filename, r)
}

func ParseString(filename, input string) (*Bitfile, error) {
	return parser.ParseString(filename, input)
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
	Pos lexer.Position

	Entries []Entry `NL* (@@ NL*)* EOF`
}

func (b *Bitfile) Position() lexer.Position { return b.Pos }

// Entry is a top-level entry in a Bitfile.
//
//sumtype:decl
type Entry interface {
	Node
	entry()
}

type Assignment struct {
	Pos lexer.Position

	Name     string   `@Ident`
	Override Override `@@?`
	Value    *Block   `"=" @@`
}

func (a *Assignment) Position() lexer.Position { return a.Pos }
func (a *Assignment) entry()                   {}
func (a *Assignment) directive()               {}

type Target struct {
	Pos lexer.Position

	Docs []string `@Comment*`

	Outputs    *RefList    `@@* ":"`
	Inputs     *RefList    `@@* NL*`
	Directives []Directive `Indent NL* (@@ NL*)* Dedent`
}

func (t *Target) Position() lexer.Position { return t.Pos }
func (*Target) entry()                     {}

type VirtualTarget struct {
	Pos lexer.Position

	Docs []string `@Comment*`

	Name       string      `"virtual" @Ident ":"`
	Inputs     *RefList    `@@* NL*`
	Directives []Directive `Indent NL* (@@ NL*)* Dedent`
}

func (t *VirtualTarget) Position() lexer.Position { return t.Pos }
func (*VirtualTarget) entry()                     {}

type Template struct {
	Pos lexer.Position

	Docs []string `@Comment*`

	Name       string       `"template" @Ident`
	Parameters []*Parameter `"(" @@ ("," @@)* ")"`
	Outputs    *RefList     `@@* ":"`
	Inputs     *RefList     `@@* NL*`
	Directives []Directive  `Indent NL* (@@ NL*)* Dedent`
}

func (t *Template) Position() lexer.Position { return t.Pos }
func (*Template) entry()                     {}

// Directive is a directive in a target.
//
//sumtype:decl
type Directive interface {
	Node
	directive()
}

type Inherit struct {
	Pos lexer.Position

	Target     string      `"<" @Ident`
	Parameters []*Argument `("(" @@ ("," @@)* ")")?`
}

func (i *Inherit) Position() lexer.Position { return i.Pos }
func (i *Inherit) directive()               {}

// RefCommand is a command that takes a list of references as its value.
//
// Currently, this includes "inputs" and "outputs".
type RefCommand struct {
	Pos lexer.Position

	Override Override `@@?`
	Command  string   `@("inputs"|"outputs")`
	Value    *RefList `( ":" ((Indent (NL* @@)+ Dedent) | @@+) )?`
}

func (r *RefCommand) Position() lexer.Position { return r.Pos }
func (r *RefCommand) directive()               {}

type Command struct {
	Pos lexer.Position

	Override Override `@@?`
	Command  string   `@Ident`
	Value    *Block   `(":" @@)?`
}

func (c *Command) Position() lexer.Position { return c.Pos }
func (c *Command) directive()               {}

type Argument struct {
	Pos lexer.Position

	Name  string  `@Ident "="`
	Value *String `@@`
}

func (a *Argument) Position() lexer.Position { return a.Pos }

type Parameter struct {
	Pos lexer.Position

	Name  string  `@Ident ("="`
	Value *String `        @@)?`
}

func (p *Parameter) Position() lexer.Position { return p.Pos }

type Dir struct {
	Pos lexer.Position

	Target *Block `"dir" ":" @@`
}

func (d *Dir) Position() lexer.Position { return d.Pos }
func (d *Dir) directive()               {}

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
	Pos lexer.Position

	Body string `WS? ((Indent NL+ @(WS | ~Dedent)+ Dedent) 
						| @(WS | ~(NL|Dedent))*)`
}

func (b *Block) Position() lexer.Position { return b.Pos }

// RefList is a list of references to file or virtual targets.
type RefList struct {
	Pos lexer.Position

	Refs []*Ref `@@+`
}

func (r *RefList) Position() lexer.Position { return r.Pos }
func (r *RefList) Strings() []string {
	if r == nil {
		return nil
	}
	strs := make([]string, len(r.Refs))
	for i, ref := range r.Refs {
		strs[i] = ref.Text
	}
	return strs
}

// Ref is a reference to a file or virtual target.
type Ref struct {
	Pos lexer.Position

	// This is a bit hairy because we need to explicitly match WS
	// to "un"-elide it, but we don't want to capture it.
	Text string `WS? ((?!WS) @(Var | Cmd | Ident | Number | "/" | "." | "*"))+ | @(String | StringLiteral | MultilineString)`
}

func (r *Ref) Position() lexer.Position { return r.Pos }
func (r *Ref) String() string           { return r.Text }

type String struct {
	Pos lexer.Position

	Value string `@(String | StringLiteral | MultilineString)`
}

func (s *String) Position() lexer.Position { return s.Pos }
func (s *String) String() string           { return s.Value }
