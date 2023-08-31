package glob

import (
	"io/fs"
	"path/filepath"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Globber is a file globber that respects .gitingore files.
type Globber struct {
	dir        string
	files      []string
	cache      map[string][]string
	extraFiles func() []string
}

// NewGlobber creates a new Globber for the given directory.
//
// The extraFiles function is called to provide additional files to be
// considered when globbing. This is useful for files output by the
// build process.
func NewGlobber(dir string, extraFiles func() []string) (*Globber, error) {
	ignore, err := GlobifyGitIgnoreFile(dir)
	if err != nil {
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
		cache:      map[string][]string{},
		extraFiles: extraFiles,
	}, nil
}

func (g *Globber) IsGlob(glob string) bool {
	return strings.ContainsAny(glob, "*?{}[]")
}

// Filepath returns a list of files matching the given glob.
func (g *Globber) Filepath(glob string) []string {
	if cached, ok := g.cache[glob]; ok {
		return cached
	}
	if !g.IsGlob(glob) {
		return []string{glob}
	}
	var matches []string
	for _, file := range g.files {
		if ok, err := doublestar.Match(glob, file); ok && err == nil {
			matches = append(matches, file)
		}
	}
	for _, file := range g.extraFiles() {
		if ok, err := doublestar.Match(glob, file); ok && err == nil {
			matches = append(matches, file)
		}
	}
	g.cache[glob] = matches
	return matches
}
