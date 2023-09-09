package internal

import (
	"io/fs"
	"path/filepath"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Globber is a file globber that respects .gitingore files.
type Globber struct {
	dir     string
	ignore  []string
	files   []string
	outputs func() []string
}

// NewGlobber creates a new Globber for the given directory.
//
// The "outputs" function is called to provide additional files to be
// considered when globbing. This is used for files output by the
// build process.
func NewGlobber(root string, outputs func() []string) (*Globber, error) {
	ignore := LoadGitIgnore(root)
	var files []string
	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if path == root {
			return nil
		}
		if d.IsDir() && path != root {
			extraIgnores := LoadGitIgnore(path)
			for _, extraIgnore := range extraIgnores {
				extraIgnore = strings.TrimPrefix(strings.TrimPrefix(filepath.Join(path, extraIgnore), root), "/")
				ignore = append(ignore, extraIgnore)
			}
		}
		path = strings.TrimPrefix(path, root+"/")
		for _, ignore := range ignore {
			if ok, err := doublestar.Match(ignore, path); ok || err != nil {
				if d.IsDir() {
					return filepath.SkipDir
				}
				return nil
			}
		}
		files = append(files, path)
		return nil
	})
	if err != nil {
		return nil, err
	}
	return &Globber{
		dir:     root,
		ignore:  ignore,
		files:   files,
		outputs: outputs,
	}, nil
}

func (g *Globber) IsGlob(glob string) bool {
	return strings.ContainsAny(glob, "*?{}[]")
}

func (g *Globber) Ignored() []string {
	return g.ignore
}

func (g *Globber) Files() []string {
	extra := g.outputs()
	out := make([]string, len(g.files), len(g.files)+len(extra))
	copy(out, g.files)
	seen := map[string]struct{}{}
	for _, file := range g.files {
		seen[file] = struct{}{}
	}
	for _, file := range extra {
		if _, ok := seen[file]; ok {
			continue
		}
		seen[file] = struct{}{}
		out = append(out, file)
	}
	return out
}

// MatchFilesystem returns a list of files matching the given glob.
func (g *Globber) MatchFilesystem(glob string) []string {
	if !g.IsGlob(glob) {
		return []string{glob}
	}
	var matches []string
	for _, file := range g.Files() {
		if ok, err := doublestar.Match(glob, file); ok && err == nil {
			matches = append(matches, file)
		}
	}
	return matches
}
