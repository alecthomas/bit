package continuation

import (
	"strings"
	"testing"

	"github.com/alecthomas/assert/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

func TestContinuationLexer(t *testing.T) {
	var def = New(lexer.MustSimple([]lexer.SimpleRule{
		{"Ident", `[a-zA-Z_][a-zA-Z0-9_]*`},
		{"Continuation", `\s*\\\n\s*`},
		{"Punct", `[[:punct:]]`},
		{"NL", `[\n\r]+[\s\t]*`},
		{"WS", `[ \t]+`},
	}))
	lex, err := def.Lex("", strings.NewReader(`
foo \
  bar waz
`))
	assert.NoError(t, err)
	actual, err := lexer.ConsumeAll(lex)
	assert.NoError(t, err)
	for i, t := range actual {
		t.Pos = lexer.Position{}
		actual[i] = t
	}
	expected := []lexer.Token{
		{Type: -5, Value: "\n"},
		{Type: -2, Value: "foo"},
		{Type: -6, Value: " "},
		{Type: -2, Value: "bar"},
		{Type: -6, Value: " "},
		{Type: -2, Value: "waz"},
		{Type: -5, Value: "\n"},
		{Type: -1, Value: ""},
	}
	assert.Equal(t, expected, actual)
}
