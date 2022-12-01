package golang

import (
	"fmt"
	"path/filepath"
	"strings"

	"github.com/Duncaen/go-ninja"
)

type GoCmdAnalyser struct{}

// GoCmd returns an Analyser that generates Ninja rules for compiling Go executables.
func GoCmd() *GoCmdAnalyser {
	return &GoCmdAnalyser{}
}

func (GoCmdAnalyser) Patterns() []string {
	return []string{`.*\bmain\.go$`}
}

func (GoCmdAnalyser) Setup() []ninja.Node {
	return []ninja.Node{
		ninja.Rule{
			Name:    "gocmd",
			Command: `go build -trimpath -buildvcs=false -ldflags="-s -w -buildid=" -o $out ./$dir`,
		},
	}
}

func (GoCmdAnalyser) Analyse(file string) ([]ninja.Node, error) {
	dir := filepath.Dir(file)
	module, err := getModulePath(file)
	if err != nil {
		return nil, fmt.Errorf("failed to find Go module: %w", err)
	}
	files, err := collectGoFiles(module, dir, map[string]bool{})
	if err != nil {
		return nil, fmt.Errorf("failed to collect Go files: %w", err)
	}
	build := ninja.Build{
		Rule: "gocmd",
		In:   files,
		Out:  []string{filepath.Join("$dest", filepath.Base(dir))},
		Vars: ninja.Vars{{"dir", dir}},
	}
	return []ninja.Node{build}, nil
}

func collectGoFiles(module, dir string, seen map[string]bool) ([]string, error) {
	files := []string{}
	pkgs, err := fastParseDir(dir)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", dir, err)
	}
	for _, pkg := range pkgs {
		if pkg.Name != "main" && len(seen) == 0 {
			return nil, nil
		}
		for path, file := range pkg.Files {
			files = append(files, path)
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
