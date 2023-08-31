package glob

import (
	"io/fs"
	"path/filepath"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Globber is a file globber that respects .gitingore files.
type Globber struct {
	dir   string
	files []string
	cache map[string][]string
}

func NewGlobber(dir string) (*Globber, error) {
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
	return &Globber{dir: dir, files: files, cache: map[string][]string{}}, nil
}

// Filepath returns a list of files matching the given glob.
func (g *Globber) Filepath(glob string) []string {
	if cached, ok := g.cache[glob]; ok {
		return cached
	}
	if !strings.ContainsAny(glob, "*?{}[]") {
		return []string{glob}
	}
	var matches []string
	for _, file := range g.files {
		if ok, err := doublestar.Match(glob, file); ok && err == nil {
			matches = append(matches, file)
		}
	}
	g.cache[glob] = matches
	return matches
}
