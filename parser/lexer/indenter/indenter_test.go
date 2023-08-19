package indenter

import (
	"strings"
	"testing"

	"github.com/alecthomas/assert/v2"
	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

func TestIndenter(t *testing.T) {
	var def = New(lexer.MustSimple([]lexer.SimpleRule{
		{"Ident", `[a-zA-Z_][a-zA-Z0-9_]*`},
		{"NL", `[\n\r]+[\s\t]*`},
	}))
	lex, err := def.Lex("", strings.NewReader(strings.TrimSpace(`
foo
  bar
  waz
    baz
  quux
foz`)))
	identType := def.Symbols()["Ident"]
	indentType := def.Symbols()["Indent"]
	dedentType := def.Symbols()["Dedent"]
	nlType := def.Symbols()["NL"]

	assert.NoError(t, err)
	tokens, err := lexer.ConsumeAll(lex)
	assert.NoError(t, err)
	for i, t := range tokens {
		t.Pos = lexer.Position{}
		tokens[i] = t
	}
	assert.Equal(t, []lexer.Token{
		{Type: identType, Value: "foo"},
		{Type: indentType, Value: "⇥"},
		{Type: nlType, Value: "\n"},
		{Type: identType, Value: "bar"},
		{Type: nlType, Value: "\n"},
		{Type: identType, Value: "waz"},
		{Type: indentType, Value: "⇥"},
		{Type: nlType, Value: "\n"},
		{Type: identType, Value: "baz"},
		{Type: dedentType, Value: "⇤"},
		{Type: nlType, Value: "\n"},
		{Type: identType, Value: "quux"},
		{Type: dedentType, Value: "⇤"},
		{Type: nlType, Value: "\n"},
		{Type: identType, Value: "foz"},
		{Type: nlType, Value: "\n"},
		{Type: -1, Value: ""},
	}, tokens)
}

func TestIndenterTrailing(t *testing.T) {
	var def = New(lexer.MustSimple([]lexer.SimpleRule{
		{"Ident", `[a-zA-Z_][a-zA-Z0-9_]*`},
		{"Number", `[0-9]+`},
		{"Punct", `[=.]`},
		{"NL", `[\n\r][\s\t]*`},
		{"WS", `[ \t]+`},
	}))
	lex, err := def.Lex("", strings.NewReader(`
dest = build
version =
  1.2.3`))
	assert.NoError(t, err)
	tokens, err := lexer.ConsumeAll(lex)
	assert.NoError(t, err)
	for i := range tokens {
		tokens[i].Pos = lexer.Position{}
	}
	expected := []lexer.Token{
		{Type: -5, Value: "\n"},
		{Type: -2, Value: "dest"},
		{Type: -6, Value: " "},
		{Type: -4, Value: "="},
		{Type: -6, Value: " "},
		{Type: -2, Value: "build"},
		{Type: -5, Value: "\n"},
		{Type: -2, Value: "version"},
		{Type: -6, Value: " "},
		{Type: -4, Value: "="},
		{Type: -7, Value: "⇥"},
		{Type: -5, Value: "\n"},
		{Type: -3, Value: "1"},
		{Type: -4, Value: "."},
		{Type: -3, Value: "2"},
		{Type: -4, Value: "."},
		{Type: -3, Value: "3"},
		{Type: -8, Value: "⇤"},
		{Type: -5, Value: "\n"},
		{Type: -1, Value: ""},
	}

	assert.Equal(t, expected, tokens)
}

func TestParseIndentedLanguage(t *testing.T) {
	var lex = New(lexer.MustSimple([]lexer.SimpleRule{
		{"Ident", `[a-zA-Z_][a-zA-Z0-9_]*`},
		{"Punct", `[[:punct:]]`},
		{"NL", `[\n\r]+[\s\t]*`},
		{"Whitespace", `[ \t]+`},
	}))

	type Stmt struct {
		Print string `( "print" @Ident`
		Log   string `| "log" @Ident)`
	}

	type Block struct {
		Stmts []Stmt `Indent @@* Dedent`
	}

	type If struct {
		Condition string `"if" @Ident ":"`
		Block     *Block `@@`
		Else      *Block `("else" ":" @@)?`
	}

	var parser = participle.MustBuild[If](participle.Lexer(lex), participle.Elide("NL", "Whitespace"))
	ast, err := parser.ParseString("", `

if foo:
  print bar
else:
  log waz

`)
	assert.NoError(t, err)
	expected := &If{
		Condition: "foo",
		Block:     &Block{Stmts: []Stmt{{Print: "bar"}}},
		Else:      &Block{Stmts: []Stmt{{Log: "waz"}}},
	}
	assert.Equal(t, expected, ast)
}
