package internal

import (
	"errors"
	"io/fs"
	"path/filepath"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Globber is a file globber that respects .gitingore files.
type Globber struct {
	dir        string
	files      []string
	extraFiles func() []string
}

// NewGlobber creates a new Globber for the given directory.
//
// The extraFiles function is called to provide additional files to be
// considered when globbing. This is useful for files output by the
// build process.
func NewGlobber(dir string, extraFiles func() []string) (*Globber, error) {
	ignore, err := GlobifyGitIgnoreFile(dir)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return nil, err
	}
	for i, glob := range ignore {
		if strings.HasPrefix(glob, "!") {
			ignore[i] = glob[1:]
			continue
		}
	}
	var files []string
	err = filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		for _, ignore := range ignore {
			if ok, err := doublestar.Match(ignore, path); ok || err != nil {
				if d.IsDir() {
					return filepath.SkipDir
				}
				return nil
			}
		}
		path = strings.TrimPrefix(path, dir+"/")
		files = append(files, path)
		return nil
	})
	if err != nil {
		return nil, err
	}
	return &Globber{
		dir:        dir,
		files:      files,
		extraFiles: extraFiles,
	}, nil
}

func (g *Globber) IsGlob(glob string) bool {
	return strings.ContainsAny(glob, "*?{}[]")
}

func (g *Globber) Files() []string {
	extra := g.extraFiles()
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
