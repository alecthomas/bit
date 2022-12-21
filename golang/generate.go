package golang

import (
	"fmt"
	"go/ast"
	"path/filepath"
	"strings"

	"github.com/alecthomas/bit"
)

func NewGenerateAnalyser() bit.Analyser {
	return GenerateAnalyser{}
}

type GenerateAnalyser struct {
}

func (GenerateAnalyser) Patterns() (match, exclude []string) {
	return []string{`**/*.go`}, Excludes
}

func (GenerateAnalyser) Analyse(ctx bit.Context, file string) (bit.Rule, error) {
	inputs := []bit.Input{}
	afile, err := ParseFile(file)
	if err != nil {
		return bit.Rule{}, fmt.Errorf("failed to parse %s: %w", file, err)
	}
	ast.Inspect(afile, func(n ast.Node) bool {
		comment, ok := n.(*ast.Comment)
		if !ok || !strings.HasPrefix(comment.Text, "//go:generate") {
			return true
		}
		inputs = append(inputs, bit.File(file))
		return false
	})
	return bit.Rule{
		Inputs:  inputs,
		Command: `go generate -x ` + file,
		Watch:   []string{filepath.Dir(file)},
	}, nil
}
