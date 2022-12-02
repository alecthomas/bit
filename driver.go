package bit

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/Duncaen/go-ninja"
)

type genOpts struct {
	dir  string
	dest string
}

type Option func(o *genOpts)

func WithDest(dir string) Option {
	return func(o *genOpts) {
		o.dest = dir
	}
}

// Generate Ninja build file using provided analysers.
func Generate(w io.Writer, analysers []Analyser, options ...Option) error {
	opts := genOpts{dir: ".", dest: "build"}
	for _, opt := range options {
		opt(&opts)
	}
	files := []string{}
	err := filepath.WalkDir(opts.dir, func(path string, d os.DirEntry, err error) error {
		if d.IsDir() && (path != opts.dir && (strings.HasPrefix(path, ".") || strings.Contains(path, "/."))) {
			return filepath.SkipDir
		}
		files = append(files, path)
		return nil
	})
	if err != nil {
		return fmt.Errorf("failed to walk directory %s: %w", opts.dir, err)
	}
	type match struct {
		analyser Analyser
		include  *regexp.Regexp
		exclude  *regexp.Regexp
	}
	file := ninja.File{ninja.Vars{
		{"dest", opts.dest},
	}}
	matches := []match{}
	using := map[Analyser]bool{}
	for _, a := range analysers {
		using[a] = true
		include, exclude := a.Patterns()
		includePattern := "(?:" + strings.Join(include, ")|(?:") + ")"
		includeRe, err := regexp.Compile(includePattern)
		if err != nil {
			return fmt.Errorf("failed to compile include pattern %s: %w", includePattern, err)
		}
		excludePattern := "(?:" + strings.Join(exclude, ")|(?:") + ")"
		excludeRe, err := regexp.Compile(excludePattern)
		if err != nil {
			return fmt.Errorf("failed to compile exclude pattern %s: %w", excludePattern, err)
		}
		matches = append(matches, match{analyser: a, include: includeRe, exclude: excludeRe})
	}

	for a := range using {
		file = append(file, a.Setup()...)
	}

	for _, m := range matches {
		for _, f := range files {
			if m.exclude.MatchString(f) || !m.include.MatchString(f) {
				continue
			}
			nodes, err := m.analyser.Analyse(f)
			if err != nil {
				return fmt.Errorf("failed to analyse %s: %w", f, err)
			}
			file = append(file, nodes...)
		}
	}

	_, err = file.WriteTo(w)
	if err != nil {
		return fmt.Errorf("failed to write Ninja file: %w", err)
	}
	return nil
}
