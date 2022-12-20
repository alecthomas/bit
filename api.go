package bit

import (
	"crypto/sha256"
	"fmt"
	"net/url"
	"os"
	"path/filepath"

	"github.com/kballard/go-shellquote"
)

// An Input is a file or other resource that is an input to a build rule.
type Input struct {
	URI  url.URL
	Hash [32]byte
}

func (i Input) String() string {
	return i.URI.String()
}

func (i Input) GoString() string {
	return fmt.Sprintf("{URI: %q, Hash: %x}", i.URI.String(), i.Hash)
}

type Vars map[string]string

// A Rule in the build graph.
//
// A Rule generates outputs from inputs by executing a command.
type Rule struct {
	Inputs  []Input
	Vars    Vars
	Command string
	// Predicted outputs, if any. If outputs are not known ahead of time, they
	// will be collected by monitoring the file system for changes when the
	// command executes.
	Outputs []string
	// If present, these paths will be watched for changes when the command is
	// executed.
	Watch []string
}

// Valid returns true if the rule is valid.
func (r Rule) Valid() bool {
	return len(r.Inputs) != 0
}

// Context for a build.
type Context struct {
	// Root directory of the build.
	Root string
	// Destination directory for generated artefacts.
	Dest string
}

func (c Context) Expand(rule Rule, s string) (string, error) {
	var err error
	out := os.Expand(s, func(key string) string {
		switch key {
		case "root":
			return c.Root
		case "dest":
			return c.Dest
		case "inputs":
			inputs := make([]string, len(rule.Inputs))
			for i, input := range rule.Inputs {
				if input.URI.Scheme == "file" {
					inputs[i] = input.URI.Path
				} else {
					// TODO: Cache non-file inputs locally.
					inputs[i] = input.URI.String()
				}
			}
			return shellquote.Join(inputs...)
		default:
			value, ok := rule.Vars[key]
			if ok {
				return value
			}
			err = fmt.Errorf("unknown variable: %s", key)
			return "${" + key + "}"
		}
	})
	return out, err
}

// Analyser is a type that can analyse a file and return a build rule.
type Analyser interface {
	// Patterns returns a set of globs that, if matched against files, will trigger the Analyser.
	Patterns() (match, exclude []string)
	// Analyse is called with each file matching the regular expressions returned by [Patterns].
	Analyse(ctx Context, file string) (Rule, error)
}

// File returns a Resource for the given path.
func File(path string) Input {
	var hash [32]byte
	info, err := os.Stat(path)
	if err == nil {
		hash = sha256.Sum256([]byte(info.ModTime().String()))
	}
	abs, err := filepath.Abs(path)
	if err != nil {
		abs = path
	}
	uri := url.URL{Scheme: "file", Path: abs}
	return Input{URI: uri, Hash: hash}
}

// Glob returns an Input representing matching files for a glob pattern.
func Glob(glob string) (Input, error) {
	matches, err := filepath.Glob(glob)
	if err != nil {
		return Input{}, err
	}
	hash := sha256.New()
	for _, match := range matches {
		info, err := os.Stat(match)
		if err != nil {
			return Input{}, fmt.Errorf("%s: %w", match, err)
		}
		hash.Write([]byte(info.ModTime().String()))
	}
	uri := url.URL{Scheme: "file", Path: glob}
	input := Input{URI: uri}
	copy(input.Hash[:], hash.Sum(nil))
	return input, nil
}
