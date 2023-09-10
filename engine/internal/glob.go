package internal

import (
	"io/fs"
	"path/filepath"
	"sort"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Globber is a file globber that respects .gitingore files.
type Globber struct {
	dir    string
	ignore []string
	files  []string
	// Doesn't cache outputs
	cache   map[string][]string
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
	sort.Strings(files)
	return &Globber{
		dir:     root,
		ignore:  ignore,
		files:   files,
		outputs: outputs,
		cache:   map[string][]string{},
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
	prefix, _ := doublestar.SplitPattern(glob)
	if prefix == "." {
		prefix = ""
	}
	var matches []string

	// start := time.Now()

	// Try and load from cache. On a large monorepo This can significantly speed
	// up matching.
	if value, ok := g.cache[glob]; ok {
		matches = make([]string, len(value))
		copy(matches, value)
	} else {
		// We've sorted the files, so we can do a binary search to find the
		// start. This is still not ideal though, as we're still iterating over
		// the entire range of files matching the prefix. For example a glob
		// like "apps/*/cmd/*" Will still have to iterate over all files in "apps".
		// If we stored the file list in a tree format, we could speed this up
		// significantly.
		start := sort.SearchStrings(g.files, prefix)
		for i := start; i < len(g.files) && strings.HasPrefix(g.files[i], prefix); i++ {
			file := g.files[i]
			if ok, err := doublestar.Match(glob, file); ok && err == nil {
				matches = append(matches, file)
			}
		}
		g.cache[glob] = matches
	}
	// fmt.Println(glob, time.Since(start))
	for _, file := range g.outputs() {
		if ok, err := doublestar.Match(glob, file); ok && err == nil {
			matches = append(matches, file)
		}
	}
	sort.Strings(matches)
	// Remove duplicates.
	move := 0
	for i := 1; i < len(matches); i++ {
		if matches[i] == matches[i-1] {
			move++
			continue
		}
		matches[i-move] = matches[i]
	}
	matches = matches[:len(matches)-move]
	return matches
}
