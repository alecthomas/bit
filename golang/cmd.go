package golang

import (
	"fmt"
	"path/filepath"
	"strings"

	"github.com/alecthomas/bit"
)

func NewCmdAnalyser() bit.Analyser {
	return CmdAnalyser{}
}

type CmdAnalyser struct{}

func (CmdAnalyser) Patterns() (match, exclude []string) {
	return []string{`**/main.go`}, Excludes
}

func (CmdAnalyser) Analyse(ctx bit.Context, file string) (bit.Rule, error) {
	dir := filepath.Dir(file)
	absDir, err := filepath.Abs(dir)
	if err != nil {
		return bit.Rule{}, fmt.Errorf("failed to get absolute path of %s: %w", file, err)
	}
	module, err := GetModulePath(file)
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
	pkgs, err := FastParseDir(dir)
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
