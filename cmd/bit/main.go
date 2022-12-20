package main

import (
	"fmt"
	"go/ast"
	"os"
	"path/filepath"
	"strings"

	"github.com/alecthomas/bit"
	"github.com/alecthomas/bit/golang"
	"github.com/alecthomas/kong"
)

var cli struct {
	Chdir string `short:"C" help:"Change to directory before running." default:"."`
	Dest  string `short:"d" help:"Destination directory for generated artefacts." default:"build"`
}

func main() {
	kctx := kong.Parse(&cli,
		kong.Description(`A zero-configuration build tool powered by Ninja`),
	)
	if cli.Chdir != "" {
		if err := os.Chdir(cli.Chdir); err != nil {
			kctx.FatalIfErrorf(err)
		}
	}
	err := bit.Build([]bit.Analyser{GoCmdAnalyser{}, GoGenerateAnalyser{}})
	kctx.FatalIfErrorf(err)
}

type GoGenerateAnalyser struct {
}

func (GoGenerateAnalyser) Patterns() (match, exclude []string) {
	return []string{`**/*.go`}, golang.Excludes
}

func (GoGenerateAnalyser) Analyse(ctx bit.Context, file string) (bit.Rule, error) {
	inputs := []bit.Input{}
	afile, err := golang.ParseFile(file)
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

type GoCmdAnalyser struct{}

func (GoCmdAnalyser) Patterns() (match, exclude []string) {
	return []string{`**/main.go`}, golang.Excludes
}

func (GoCmdAnalyser) Analyse(ctx bit.Context, file string) (bit.Rule, error) {
	dir := filepath.Dir(file)
	absDir, err := filepath.Abs(dir)
	if err != nil {
		return bit.Rule{}, fmt.Errorf("failed to get absolute path of %s: %w", file, err)
	}
	module, err := golang.GetModulePath(file)
	if err != nil {
		return bit.Rule{}, fmt.Errorf("failed to find Go module: %w", err)
	}
	files, err := collectGoFiles(module, dir, map[string]bool{})
	if err != nil {
		return bit.Rule{}, fmt.Errorf("failed to collect Go files: %w", err)
	}
	return bit.Rule{
		Inputs:  files,
		Command: `go build -trimpath -buildvcs=false -ldflags="-s -w -buildid=" -o $dest/$out $main`,
		Vars: bit.Vars{
			"main": dir,
			"out":  filepath.Base(dir),
		},
		Outputs: []string{filepath.Join("$dest", filepath.Base(absDir))},
	}, nil
}

func collectGoFiles(module, dir string, seen map[string]bool) ([]bit.Input, error) {
	files := []bit.Input{}
	pkgs, err := golang.FastParseDir(dir)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", dir, err)
	}
	for _, pkg := range pkgs {
		if pkg.Name != "main" && len(seen) == 0 {
			return nil, nil
		}
		for path, file := range pkg.Files {
			files = append(files, bit.File(path))
			for _, imp := range file.Imports {
				dir := imp.Path.Value[1 : len(imp.Path.Value)-1]
				if !strings.HasPrefix(dir, module+"/") {
					continue
				}
				dir = strings.TrimPrefix(dir, module+"/")
				if seen[dir] {
					continue
				}
				seen[dir] = true
				subFiles, err := collectGoFiles(module, dir, seen)
				if err != nil {
					return nil, fmt.Errorf("%s: %w", dir, err)
				}
				files = append(files, subFiles...)
			}
		}
	}
	return files, nil
}
