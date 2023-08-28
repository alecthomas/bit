package parser

import (
	"testing"

	"github.com/alecthomas/assert/v2"
)

func TestParseText(t *testing.T) {
	text, err := ParseTextString(`Hello, %{world}! What's happening %(echo "today")%?`)
	assert.NoError(t, err)
	text = normaliseNode(text)
	for i, fragment := range text.Fragments {
		text.Fragments[i] = normaliseNode(fragment)
	}
	assert.Equal(t, &Text{
		Fragments: []Fragment{
			&TextFragment{Text: "Hello, "},
			&VarFragment{Var: "world"},
			&TextFragment{Text: "! What's happening "},
			&CmdFragment{Cmd: `echo "today"`},
			&TextFragment{Text: "?"},
		},
	}, text)
}
