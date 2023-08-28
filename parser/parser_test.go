package parser

import (
	"embed"
	_ "embed"
	"io/fs"
	"os"
	"reflect"
	"strings"
	"testing"

	"github.com/alecthomas/assert/v2"
	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
	"github.com/alecthomas/repr"
	"github.com/lithammer/dedent"
)

//go:embed testdata/*
var testSamples embed.FS

func TestParser(t *testing.T) {
	tests := []struct {
		name     string
		input    string
		expected *Bitfile
		trace    bool
	}{
		{name: "Assignment",
			input: `
				dest = ./build
				version =
					%(git describe --tags --always)
				`,
			expected: &Bitfile{
				Entries: []Entry{
					&Assignment{Name: "dest", Value: &Block{Body: "./build"}},
					&Assignment{Name: "version", Value: &Block{Body: "%(git describe --tags --always)"}},
				}}},
		{name: "Target",
			input: `
				virtual k8s-postgres: some/ path
					< k8s-apply(manifest="db.yml", resource="pod/ftl-pg-cluster-1-0")
					dir:
						./db

				target:
					< k8s-postgres
				`,
			expected: &Bitfile{
				Entries: []Entry{
					&VirtualTarget{
						Name:   "k8s-postgres",
						Inputs: &RefList{Refs: []*Ref{{Text: "some/"}, {Text: "path"}}},
						Directives: []Directive{
							&Inherit{
								Target: "k8s-apply",
								Parameters: []*Argument{
									{Name: "manifest", Value: &String{Value: `"db.yml"`}},
									{Name: "resource", Value: &String{Value: `"pod/ftl-pg-cluster-1-0"`}},
								},
							},
							&Dir{
								Target: &Block{Body: "./db"},
							},
						},
					},
					&Target{
						Outputs: &RefList{Refs: []*Ref{{Text: "target"}}},
						Directives: []Directive{
							&Inherit{Target: "k8s-postgres"},
						},
					},
				},
			}},
		{name: "Overrides",
			input: `
				virtual docker-postgres:
					< docker
					+inputs: src
					-outputs
					^inputs: another
				`,
			expected: &Bitfile{
				Entries: []Entry{
					&VirtualTarget{
						Name: "docker-postgres",
						Directives: []Directive{
							&Inherit{Target: "docker"},
							&RefCommand{
								Override: OverrideAppend,
								Command:  "inputs",
								Value:    &RefList{Refs: []*Ref{{Text: "src"}}},
							},
							&RefCommand{
								Override: OverrideDelete,
								Command:  "outputs",
							},
							&RefCommand{
								Override: OverridePrepend,
								Command:  "inputs",
								Value:    &RefList{Refs: []*Ref{{Text: "another"}}},
							},
						},
					},
				},
			},
		},
		{name: "VirtualWithOutputs",
			input: `
				virtual k8s-ftl-controller: k8s-postgres
				  < k8s-apply(manifest="ftl-controller.yml", resource="deployment/ftl-controller")
				  dir: ./ftl-controller
			`,
			expected: &Bitfile{
				Entries: []Entry{
					&VirtualTarget{
						Name:   "k8s-ftl-controller",
						Inputs: &RefList{Refs: []*Ref{{Text: "k8s-postgres"}}},
						Directives: []Directive{
							&Inherit{
								Target: "k8s-apply",
								Parameters: []*Argument{
									{Name: "manifest", Value: &String{Value: `"ftl-controller.yml"`}},
									{Name: "resource", Value: &String{Value: `"deployment/ftl-controller"`}},
								},
							},
							&Dir{
								Target: &Block{Body: "./ftl-controller"},
							},
						},
					},
				},
			},
		},
		{name: "Template",
			input: `
				template go-cmd(pkg):
				  inputs: %(go list -f '{{ join .Deps "\n" }}' %{pkg} | grep github.com/TBD54566975/ftl | cut -d/ -f4-)%
				  build: go build -tags release -ldflags "-X main.version=%{version}" -o %{output} %{pkg}
				`,
			expected: &Bitfile{
				Entries: []Entry{
					&Template{
						Name:       "go-cmd",
						Parameters: []*Parameter{{Name: "pkg"}},
						Directives: []Directive{
							&RefCommand{
								Command: "inputs",
								Value: &RefList{Refs: []*Ref{
									{Text: "%(go list -f '{{ join .Deps \"\\n\" }}' %{pkg} | grep github.com/TBD54566975/ftl | cut -d/ -f4-)%"},
								}},
							},
							&Command{
								Command: "build",
								Value: &Block{
									Body: "go build -tags release -ldflags \"-X main.version=%{version}\" -o %{output} %{pkg}",
								},
							},
						},
					},
				},
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			input := strings.TrimSpace(dedent.Dedent(test.input))
			tokens := tokenise(t, input)
			var options []participle.ParseOption
			if test.trace {
				options = append(options, participle.Trace(os.Stderr))
			}
			bitfile, err := parser.ParseString("", input, options...)
			assert.NoError(t, err, "%s\n%s", repr.String(tokens, repr.Indent("  ")), repr.String(bitfile, repr.Indent("  ")))
			_ = Visit(bitfile, func(node Node, next func() error) error {
				normaliseNode(node)
				return next()
			})
			assert.Equal(t, test.expected, bitfile)
		})
	}
}

func normaliseNode[T any](node T) T {
	v := reflect.Indirect(reflect.ValueOf(node))
	f := v.FieldByName("Pos")
	if !f.CanAddr() {
		panic(node)
	}
	f.Set(reflect.Zero(f.Type()))
	return node
}

func tokenise(t *testing.T, input string) []lexer.Token {
	t.Helper()
	lex, err := lex.Lex("", strings.NewReader(input))
	assert.NoError(t, err)
	tokens, err := lexer.ConsumeAll(lex)
	assert.NoError(t, err)
	return tokens
}

func TestParseSamples(t *testing.T) {
	testdata, err := fs.Sub(testSamples, "testdata")
	assert.NoError(t, err)
	examples, err := fs.ReadDir(testdata, ".")
	assert.NoError(t, err)
	for _, example := range examples {
		t.Run(example.Name(), func(t *testing.T) {
			input, err := fs.ReadFile(testdata, example.Name())
			assert.NoError(t, err)
			// tokens := tokenise(t, string(input))
			bitfile, err := ParseString(example.Name(), string(input))
			assert.NoError(t, err, "%s\n%s", repr.String(bitfile, repr.Indent("  ")) /*, repr.String(tokens, repr.Indent("  "))*/)
		})
	}
}
