// Package indenter provides a lexer that inserts INDENT and DEDENT
// tokens based on indentation.
//
// It relies on the parent lexer to provide an "NL" token that matches
// newlines followed by whitespace.
package indenter

import (
	"io"
	"maps"
	"strings"

	"github.com/alecthomas/participle/v2/lexer"
)

func New(parent lexer.Definition) lexer.Definition {
	out := maps.Clone(parent.Symbols())
	if _, ok := out["NL"]; !ok {
		panic("parent lexer must have an NL symbol similar to \"[\\n\\r]+[\\s\\t]*\"")
	}
	if _, ok := out["Indent"]; ok {
		panic("parent lexer must not have an Indent symbol")
	}
	if _, ok := out["Dedent"]; ok {
		panic("parent lexer must not have a Dedent symbol")
	}
	next := lexer.TokenType(-1)
	for _, t := range out {
		if t <= next {
			next = t - 1
		}
	}
	out["Indent"] = next
	out["Dedent"] = next - 1
	return &indentLexerDef{parent: parent, symbols: out}
}

type indentLexerDef struct {
	parent  lexer.Definition
	symbols map[string]lexer.TokenType
}

func (i *indentLexerDef) Symbols() map[string]lexer.TokenType {
	return i.symbols
}

func (i *indentLexerDef) Lex(filename string, r io.Reader) (lexer.Lexer, error) {
	lex, err := i.parent.Lex(filename, r)
	if err != nil {
		return nil, err
	}
	return &indentLexer{
		lexer:      lex,
		nlType:     i.symbols["NL"],
		indentType: i.symbols["Indent"],
		dedentType: i.symbols["Dedent"],
	}, nil
}

var _ lexer.Definition = (*indentLexerDef)(nil)

type indentLexer struct {
	nlType     lexer.TokenType
	indentType lexer.TokenType
	dedentType lexer.TokenType

	indents  []string
	eof      bool
	buffered []lexer.Token
	lexer    lexer.Lexer
}

func (i *indentLexer) Next() (lexer.Token, error) {
	if len(i.buffered) > 0 {
		t := i.buffered[0]
		i.buffered = i.buffered[1:]
		return t, nil
	}

	t, err := i.lexer.Next()
	if err != nil {
		return t, err
	}
	if t.EOF() {
		if i.eof {
			return t, err
		}
		// Always ensure we have a trailing newline to trigger dedents.
		// Without this every time the parser matched a Dedent it would also need to match an EOF, eg. `(Dedent | EOF)`
		t = lexer.Token{Pos: t.Pos, Type: i.nlType, Value: "\n"}
		i.eof = true
	} else if t.Type != i.nlType {
		return t, err
	}

	nlPos := t.Pos
	var appended []lexer.Token
	for j := 0; j < strings.Count(t.Value, "\n"); j++ {
		appended = append(appended, lexer.Token{Pos: nlPos, Type: i.nlType, Value: "\n"})
		nlPos.Line++
		nlPos.Column = 1
	}

	// At this point we have an NL symbol. Figure out if we need to
	// insert Indent tokens or Dedent tokens.
	indent := strings.TrimLeft(t.Value, "\n\r")
	pos := t.Pos
	for j, ind := range i.indents {
		if strings.HasPrefix(indent, ind) {
			indent = indent[len(ind):]
			pos.Column += len(ind)
			pos.Offset += len(ind)
		} else {
			k := len(i.indents) - j
			i.indents = i.indents[:j]
			for ; k > 0; k-- {
				i.buffered = append(i.buffered, lexer.Token{Pos: pos, Type: i.dedentType, Value: "⇤"})
			}
			break
		}
	}
	if indent != "" {
		i.indents = append(i.indents, indent)
		i.buffered = append(i.buffered, lexer.Token{Pos: pos, Type: i.indentType, Value: "⇥"})
	}
	i.buffered = append(i.buffered, appended...)
	t = i.buffered[0]
	i.buffered = i.buffered[1:]
	return t, nil
}

var _ lexer.Lexer = (*indentLexer)(nil)
