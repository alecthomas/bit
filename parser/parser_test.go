package parser

import (
	"embed"
	"io/fs"
	"os"
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
							&Command{
								Override: OverrideAppend,
								Command:  "inputs",
								Value:    &Block{Body: "src"},
							},
							&Command{
								Override: OverrideDelete,
								Command:  "outputs",
							},
							&Command{
								Override: OverridePrepend,
								Command:  "inputs",
								Value:    &Block{Body: "another"},
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
							&Command{
								Command: "inputs",
								Value:   &Block{Body: "%(go list -f '{{ join .Deps \"\\n\" }}' %{pkg} | grep github.com/TBD54566975/ftl | cut -d/ -f4-)%"},
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
		{name: "SmallBitfile",
			input: `
				%{DEST}/bit:
				  inputs:
				    %{DEST}
				    **/*.go
			`,
			expected: &Bitfile{
				Entries: []Entry{
					&Target{
						Outputs: &RefList{
							Refs: []*Ref{{Text: "%{DEST}/bit"}},
						},
						Directives: []Directive{
							&Command{Command: "inputs", Value: &Block{Body: "%{DEST}\n**/*.go"}},
						},
					},
				},
			},
		},
		{name: "SmallBitfileWithTargetDocs",
			input: `
				# This is a comment
				# This is another comment
				%{DEST}/bit:
				  build: foo
				`,
			expected: &Bitfile{
				Entries: []Entry{
					&Target{
						Docs: &Docs{Docs: "This is a comment\nThis is another comment"},
						Outputs: &RefList{
							Refs: []*Ref{{Text: "%{DEST}/bit"}},
						},
						Directives: []Directive{
							&Command{Command: "build", Value: &Block{Body: "foo"}},
						},
					},
				},
			},
		},
		{name: "BitfileWithDocs",
			input: `
				# This is a comment
				# This is another comment

				%{DEST}/bit:
				  build: foo
				`,
			expected: &Bitfile{
				Docs: &Docs{Docs: "This is a comment\nThis is another comment"},
				Entries: []Entry{
					&Target{
						Outputs: &RefList{
							Refs: []*Ref{{Text: "%{DEST}/bit"}},
						},
						Directives: []Directive{
							&Command{Command: "build", Value: &Block{Body: "foo"}},
						},
					},
				},
			},
		},
		{name: "EmptyBitfileWithDocs",
			input: `
				# This is a comment
				# This is another comment`,
			expected: &Bitfile{
				Docs: &Docs{Docs: "This is a comment\nThis is another comment"},
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
			assert.Equal(t, test.expected, bitfile, repr.String(tokens, repr.Indent("  ")), assert.Exclude[lexer.Position]())
		})
	}
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

func TestParseRefList(t *testing.T) {
	refs, err := ParseRefList(lexer.Position{}, `a b c`)
	assert.NoError(t, err)
	assert.Equal(t, &RefList{Refs: []*Ref{{Text: "a"}, {Text: "b"}, {Text: "c"}}}, refs, assert.Exclude[lexer.Position]())
}
