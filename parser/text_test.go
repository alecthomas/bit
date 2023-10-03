package parser

import (
	"testing"

	"github.com/alecthomas/assert/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

func TestParseText(t *testing.T) {
	text, err := ParseTextString(`Hello, %{world}! What's happening %(echo "today")%?`)
	assert.NoError(t, err)
	assert.Equal(t, &Text{
		Fragments: []Fragment{
			&TextFragment{Text: "Hello, "},
			&VarFragment{Var: "world"},
			&TextFragment{Text: "! What's happening "},
			&CmdFragment{Cmd: `echo "today"`},
			&TextFragment{Text: "?"},
		},
	}, text, assert.Exclude[lexer.Position]())
}
