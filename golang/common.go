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
	FileSet = token.NewFileSet()
	// Excludes are file patterns to exclude from analysis for Go files.
	Excludes = []string{`**/vendor/**`, `**/testdata/**`}
	cache    sync.Map
)

// GetModulePath of a file by searching for the go.mod file in parent
// directories.
func GetModulePath(file string) (string, error) {
	dir, err := filepath.Abs(filepath.Dir(file))
	if err != nil {
		return "", fmt.Errorf("failed to get absolute path of %s: %w", file, err)
	}
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

func ParseFile(file string) (*ast.File, error) {
	pkgs, err := ParseDir(filepath.Dir(file))
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", file, err)
	}
	for _, pkg := range pkgs {
		for filename, fast := range pkg.Files {
			if filename == file {
				return fast, nil
			}
		}
	}
	return nil, fmt.Errorf("failed to find %s in parsed files", file)
}

// ParseDir parses the full AST of all files in a directory.
func ParseDir(dir string) (pkgs map[string]*ast.Package, err error) {
	cached, ok := cache.Load(dir)
	if ok {
		return cached.(map[string]*ast.Package), nil
	}
	pkgs, err = parser.ParseDir(FileSet, dir, nil, parser.ParseComments)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", dir, err)
	}
	cache.Store(dir, pkgs)
	return pkgs, err
}

// FastParseDir parses at least up to the imports of all files in a directory.
func FastParseDir(dir string) (pkgs map[string]*ast.Package, err error) {
	key := "fast:" + dir
	cached, ok := cache.Load(key)
	if ok {
		return cached.(map[string]*ast.Package), nil
	}
	cached, ok = cache.Load(dir)
	if ok {
		return cached.(map[string]*ast.Package), nil
	}
	pkgs, err = parser.ParseDir(FileSet, dir, nil, parser.ImportsOnly)
	if err != nil {
		return nil, fmt.Errorf("failed to parse %s: %w", dir, err)
	}
	cache.Store(key, pkgs)
	return pkgs, err
}
