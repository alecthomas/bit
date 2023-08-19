package continuation

import (
	"io"
	"strings"

	"github.com/alecthomas/participle/v2/lexer"
)

func New(def lexer.Definition) lexer.Definition {
	if _, ok := def.Symbols()["Continuation"]; !ok {
		panic(`parent lexer must have a Continuation symbol with optional whitespace surrounding "\\\n"`)
	}
	ws, ok := def.Symbols()["WS"]
	if !ok {
		panic(`parent lexer must have a WS symbol`)
	}
	return &continuationLexerDef{def: def, ws: ws}
}

type continuationLexerDef struct {
	def lexer.Definition
	ws  lexer.TokenType
}

func (c *continuationLexerDef) Symbols() map[string]lexer.TokenType {
	return c.def.Symbols()
}

func (c *continuationLexerDef) Lex(filename string, r io.Reader) (lexer.Lexer, error) {
	lex, err := c.def.Lex(filename, r)
	if err != nil {
		return nil, err
	}
	return &continuationLexer{lexer: lex, ws: c.ws}, nil
}

type continuationLexer struct {
	lexer lexer.Lexer
	ws    lexer.TokenType
}

func (c *continuationLexer) Next() (lexer.Token, error) {
	t, err := c.lexer.Next()
	if err != nil {
		return t, err
	}
	if strings.Trim(t.Value, " \r\t") == "\\\n" {
		t.Type = c.ws
		t.Value = " "
	}
	return t, nil
}
