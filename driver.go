package bit

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"

	"github.com/gobwas/glob"
	"github.com/kballard/go-shellquote"
)

type genOpts struct {
	root   string
	dest   string
	dryRun bool
}

type Option func(o *genOpts)

func WithDest(dir string) Option {
	return func(o *genOpts) {
		o.dest = dir
	}
}

func DryRun(dryRun bool) Option {
	return func(o *genOpts) {
		o.dryRun = dryRun
	}
}

// Build targets using Analysers.
func Build(analysers []Analyser, options ...Option) error {
	opts := genOpts{root: ".", dest: "build"}
	for _, opt := range options {
		opt(&opts)
	}
	var err error
	opts.root, err = filepath.Abs(opts.root)
	if err != nil {
		return fmt.Errorf("failed to get absolute path for %s: %w", opts.root, err)
	}
	opts.dest, err = filepath.Abs(opts.dest)
	if err != nil {
		return fmt.Errorf("failed to get absolute path for %s: %w", opts.dest, err)
	}
	files := []string{}
	err = filepath.WalkDir(opts.root, func(path string, d os.DirEntry, err error) error {
		if d.IsDir() && (path != opts.root && (strings.HasPrefix(path, ".") || strings.Contains(path, "/."))) {
			return filepath.SkipDir
		}
		files = append(files, path)
		return nil
	})
	if err != nil {
		return fmt.Errorf("failed to walk directory %s: %w", opts.root, err)
	}
	type match struct {
		analyser Analyser
		includes string
		include  []glob.Glob
		excludes string
		exclude  []glob.Glob
	}
	matches := []match{}
	using := map[Analyser]bool{}
	for _, a := range analysers {
		using[a] = true
		include, exclude := a.Patterns()
		includes := []glob.Glob{}
		for _, inc := range include {
			pattern, err := glob.Compile(inc, '/')
			if err != nil {
				return fmt.Errorf("failed to compile include pattern %s: %w", inc, err)
			}
			includes = append(includes, pattern)
		}
		excludes := []glob.Glob{}
		for _, ex := range exclude {
			pattern, err := glob.Compile(ex, '/')
			if err != nil {
				return fmt.Errorf("failed to compile exclude pattern %s: %w", ex, err)
			}
			excludes = append(excludes, pattern)
		}
		matches = append(matches, match{
			analyser: a,
			includes: strings.Join(include, "|"),
			include:  includes,
			excludes: strings.Join(exclude, "|"),
			exclude:  excludes,
		})
	}

	rules := []Rule{}
	ctx := Context{Root: opts.root, Dest: opts.dest}
	for _, m := range matches {
		for _, f := range files {
			matched := false
			for _, inc := range m.include {
				if inc.Match(f) {
					matched = true
				}
			}
			if !matched {
				continue
			}
			for _, exc := range m.exclude {
				if exc.Match(f) {
					matched = false
				}
			}
			if !matched {
				continue
			}
			rule, err := m.analyser.Analyse(ctx, f)
			if err != nil {
				return fmt.Errorf("failed to analyse %s: %w", f, err)
			}
			if !rule.Valid() {
				continue
			}
			rules = append(rules, rule)
		}
	}

	fmt.Printf("mkdir -p %s\n", shellquote.Join(opts.dest))
	err = os.MkdirAll(opts.dest, 0700)
	if err != nil {
		return fmt.Errorf("failed to create output directory %s: %w", opts.dest, err)
	}
	sort.Slice(rules, func(i, j int) bool {
		for _, l := range rules[i].Watch {
			for _, r := range rules[j].Inputs {
				if strings.HasPrefix(r.URI.Path, l) {
					return true
				}
			}
		}
		for _, l := range rules[j].Outputs {
			l = filepath.Dir(l)
			for _, r := range rules[i].Inputs {
				if strings.HasPrefix(r.URI.Path, l) {
					return true
				}
			}
		}
		return false
	})
	for _, rule := range rules {
		// repr.Println(rule)
		command, err := ctx.Expand(rule, rule.Command)
		if err != nil {
			return fmt.Errorf("failed to expand command (%w): %s", err, rule.Command)
		}
		args, err := shellquote.Split(command)
		if err != nil {
			return fmt.Errorf("failed to parse command %q: %w", command, err)
		}
		fmt.Println(command)
		if opts.dryRun {
			continue
		}

		cmd := exec.Command(args[0], args[1:]...)
		cmd.Stdout = os.Stdout
		cmd.Stderr = os.Stderr
		err = cmd.Run()
		if err != nil {
			return fmt.Errorf("%s -- %w", command, err)
		}
	}
	return nil
}
