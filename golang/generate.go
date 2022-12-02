package golang

import (
	"encoding/hex"
	"fmt"
	"go/ast"
	"hash/fnv"
	"path/filepath"
	"strings"

	"github.com/Duncaen/go-ninja"
	"github.com/kballard/go-shellquote"
)

//go:generate enumer -type=Enum -json -text
type Enum int

const (
	EnumA Enum = iota
	EnumB
)

type GoGenerateMatcher func(path string, args []string) (inputs, outputs []string, err error)

type GoGenerateAnalyser struct{ matchers map[string]GoGenerateMatcher }

func GoGenerate(matchers map[string]GoGenerateMatcher) *GoGenerateAnalyser {
	combined := map[string]GoGenerateMatcher{
		"enumer": enumerParser,
	}
	for key, matcher := range matchers {
		combined[key] = matcher
	}
	return &GoGenerateAnalyser{matchers: combined}
}

func (GoGenerateAnalyser) Setup() []ninja.Node { return nil }
func (GoGenerateAnalyser) Patterns() (match, exclude []string) {
	return []string{`.*\.go$`}, goExcludes
}
func (g GoGenerateAnalyser) Analyse(file string) ([]ninja.Node, error) {
	dir := filepath.Dir(file)
	pkgs, err := parseDir(dir)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", file, err)
	}
	generators := []*ast.Comment{}
	for _, pkg := range pkgs {
		for path, f := range pkg.Files {
			if path != file {
				continue
			}
			for _, comment := range f.Comments {
				for _, line := range comment.List {
					if strings.HasPrefix(line.Text, "//go:generate") {
						generators = append(generators, line)
						break
					}
				}
			}
		}
	}
	ruleCreated := map[string]bool{}
	nodes := []ninja.Node{}
	for _, generator := range generators {
		match := false
		for _, matcher := range g.matchers {
			parts := strings.SplitN(generator.Text, " ", 2)
			args, err := shellquote.Split(parts[1])
			if err != nil {
				return nil, fmt.Errorf("failed to parse generator %q: %w", generator, err)
			}
			inputs, outputs, err := matcher(file, args)
			if err != nil {
				return nil, err
			}
			if len(inputs) == 0 && len(outputs) == 0 {
				continue
			}
			idh := fnv.New64()
			for _, part := range append(args, append(inputs, outputs...)...) {
				idh.Write([]byte(part))
			}
			id := hex.EncodeToString(idh.Sum(nil))
			match = true
			if !ruleCreated[args[0]] {
				nodes = append(nodes, ninja.Rule{
					Name:    "go-generate-" + args[0] + "-" + id,
					Command: "cd $dir && " + parts[1],
				})
			} else {
				ruleCreated[args[0]] = true
			}
			nodes = append(nodes, ninja.Build{
				Rule: "go-generate-" + args[0] + "-" + id,
				In:   inputs,
				Out:  outputs,
				Vars: ninja.Vars{{"dir", dir}},
			})
		}
		if !match {
			pos := fset.Position(generator.Pos())
			parts := strings.SplitN(generator.Text, " ", 2)
			return nil, fmt.Errorf("%s: don't have a parser for Go generator: %s", pos, parts[1])
		}
	}
	return nodes, nil
}

func enumerParser(path string, command []string) ([]string, []string, error) {
	inputs := []string{path}
	output := ""
	for i, arg := range command {
		switch {
		case strings.HasPrefix(arg, "-type="):
			output = strings.ToLower(arg[len("-type="):]) + "_enumer.go"
		case arg == "-type":
			output = strings.ToLower(command[i+1]) + "_enumer.go"
		case strings.HasPrefix(arg, "-output="):
			output = arg[len("-output="):]
		case arg == "-output":
			output = command[i+1]
		}
	}
	output = filepath.Join(filepath.Dir(path), output)
	return inputs, []string{output}, nil
}
