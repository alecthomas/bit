package golang

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"io/ioutil"
	"os"
	"path/filepath"
	"sync"

	"golang.org/x/mod/modfile"
)

var (
	fset       = token.NewFileSet()
	cache      sync.Map
	goExcludes = []string{`\bvendor/`, `\btestdata/`}
)

func getModulePath(file string) (string, error) {
	dir := filepath.Dir(file)
	for dir != "/" {
		if _, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil {
			break
		}
		dir = filepath.Dir(dir)
	}
	data, err := ioutil.ReadFile(filepath.Join(dir, "go.mod"))
	if err != nil {
		return "", fmt.Errorf("failed to read go.mod: %w", err)
	}
	module := modfile.ModulePath(data)
	if module == "" {
		return "", fmt.Errorf("failed to determine Go module")
	}
	return module, nil
}

// Parse full AST of all files in a directory.
func parseDir(dir string) (pkgs map[string]*ast.Package, err error) {
	cached, ok := cache.Load(dir)
	if ok {
		return cached.(map[string]*ast.Package), nil
	}
	pkgs, err = parser.ParseDir(fset, dir, nil, parser.ParseComments)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", dir, err)
	}
	cache.Store(dir, pkgs)
	return pkgs, err
}

// Parse only up to the imports of all files in a directory.
func fastParseDir(dir string) (pkgs map[string]*ast.Package, err error) {
	key := "fast:" + dir
	cached, ok := cache.Load(key)
	if ok {
		return cached.(map[string]*ast.Package), nil
	}
	pkgs, err = parser.ParseDir(fset, dir, nil, parser.ImportsOnly)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", dir, err)
	}
	cache.Store(key, pkgs)
	return pkgs, err
}
