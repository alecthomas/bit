package engine

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"

	"github.com/alecthomas/bit/engine/glob"
	"github.com/alecthomas/bit/parser"
)

type Vars map[string]*parser.Block

type Target struct {
	pos       lexer.Position
	inputs    *parser.RefList
	outputs   *parser.RefList
	build     *parser.Command
	hashPos   lexer.Position
	vars      Vars
	buildFunc func(logger *Logger, target *Target) error
	// Hash function for virtual targets.
	hashFunc func() (hasher, error)
	// Hash stored in the DB.
	storedHash hasher
	// Hash computed from the filesystem.
	realHash  hasher
	synthetic bool
}

type RefKey string

type Engine struct {
	cwd     string
	globber *glob.Globber
	log     *Logger
	vars    Vars
	db      *HashDB
	targets []*Target
	outputs map[RefKey]*Target
	inputs  map[RefKey]*Target
}

// Compile a Bitfile into an Engine ready to build targets.
func Compile(logger *Logger, bitfile *parser.Bitfile) (*Engine, error) {
	cachedir, err := os.UserCacheDir()
	if err != nil {
		return nil, fmt.Errorf("failed to get user cache dir: %w", err)
	}
	cwd, err := os.Getwd()
	if err != nil {
		return nil, fmt.Errorf("failed to get working directory: %w", err)
	}
	dir := filepath.Join(cachedir, "bit")
	err = os.MkdirAll(dir, 0750)
	if err != nil {
		return nil, fmt.Errorf("failed to create cache directory %q: %w", dir, err)
	}
	hash := sha256.Sum256([]byte(cwd))
	dir = filepath.Join(dir, fmt.Sprintf("%x.json", hash))
	db, err := NewHashDB(dir)
	if err != nil {
		return nil, err
	}
	globber, err := glob.NewGlobber(cwd)
	if err != nil {
		return nil, err
	}
	engine := &Engine{
		globber: globber,
		cwd:     cwd,
		log:     logger,
		db:      db,
		inputs:  map[RefKey]*Target{},
		outputs: map[RefKey]*Target{},
		vars: map[string]*parser.Block{
			"CWD": &parser.Block{
				Pos:  bitfile.Pos,
				Body: cwd,
			},
		},
	}
	root := &Target{
		pos:       bitfile.Pos,
		vars:      map[string]*parser.Block{},
		inputs:    &parser.RefList{Pos: bitfile.Pos},
		outputs:   &parser.RefList{Pos: bitfile.Pos},
		buildFunc: func(logger *Logger, target *Target) error { return nil },
		hashFunc:  func() (hasher, error) { return 0, nil },
	}
	for _, entry := range bitfile.Entries {
		switch entry := entry.(type) {
		case *parser.Target:
			target := &Target{
				pos:       entry.Pos,
				inputs:    entry.Inputs,
				outputs:   entry.Outputs,
				buildFunc: engine.defaultBuildFunc,
			}
			if entry.Inputs == nil {
				target.inputs = &parser.RefList{Pos: entry.Pos}
			}
			if entry.Outputs == nil {
				target.outputs = &parser.RefList{Pos: entry.Pos}
			}
			for _, directive := range entry.Directives {
				switch directive := directive.(type) {
				case *parser.Command:
					switch directive.Command {
					case "build":
						target.build = directive

					case "inputs", "outputs":
						refs, err := parser.ParseRefList(directive.Value.Body)
						if err != nil {
							return nil, participle.Errorf(directive.Value.Pos, "failed to parse %s: %s", directive.Command, err)
						}
						if directive.Command == "inputs" {
							target.inputs.Refs = append(target.inputs.Refs, refs.Refs...)
						} else {
							target.outputs.Refs = append(target.outputs.Refs, refs.Refs...)
						}

					default:
						panic(fmt.Sprintf("unsupported command %q", directive.Command))
					}

				default:
					panic(fmt.Sprintf("unsupported directive %T", directive))
				}
			}
			if target.build == nil {
				return nil, participle.Errorf(entry.Pos, "target has no build command")
			}
			if target.outputs == nil {
				return nil, participle.Errorf(entry.Pos, "target has no outputs")
			}
			root.inputs.Refs = append(root.inputs.Refs, target.outputs.Refs...)
			engine.targets = append(engine.targets, target)

		case *parser.Assignment:
			engine.vars[entry.Name] = entry.Value

		// case *parser.VirtualTarget:
		// case *parser.Template:
		default:
			panic(fmt.Sprintf("unsupported entry type %T", entry))
		}
	}
	engine.targets = append(engine.targets, root)

	if err := engine.evaluate(); err != nil {
		return nil, err
	}
	return engine, nil
}

func (e *Engine) Targets() []string {
	set := map[string]bool{}
	for _, target := range e.targets {
		if target.synthetic {
			continue
		}
		for _, output := range target.outputs.Strings() {
			set[output] = true
		}
	}
	out := make([]string, 0, len(set))
	for k := range set {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

func (e *Engine) Close() error {
	for _, target := range e.targets {
		for _, output := range target.outputs.Refs {
			e.db.Delete(output.Text)
			h, err := e.realRefHasher(output)
			if err != nil {
				continue
			}
			e.db.Set(output.Text, h)
		}
	}
	return e.db.Close()
}

func (e *Engine) Build(name string) error {
	name = e.normalisePath(name)
	log := e.log.Scope(name)

	target, err := e.getTarget(name)
	if err != nil {
		return err
	}

	if target.storedHash == target.realHash {
		log.Debugf("Up to date.")
		return nil
	}
	log.Tracef("Building.")

	// Build dependencies.
	for _, input := range target.inputs.Refs {
		if err := e.Build(input.Text); err != nil {
			return participle.Wrapf(input.Pos, err, "build failed")
		}
	}

	// Build target.
	err = target.buildFunc(log, target)
	if err != nil {
		return participle.Wrapf(target.build.Pos, err, "build failed")
	}

	h, err := e.computeHash(target, e.realRefHasher)
	if err != nil {
		return err
	}
	target.storedHash = h
	target.realHash = h
	return nil
}

func (e *Engine) defaultBuildFunc(log *Logger, target *Target) error {
	block := target.build.Value
	command, err := e.evaluateString(block.Pos, block.Body, target, map[string]bool{})
	if err != nil {
		return participle.Wrapf(block.Pos, err, "invalid command %q", command)
	}
	err = log.Exec(command)
	if err != nil {
		return participle.Wrapf(block.Pos, err, "failed to run command %q", command)
	}
	return nil
}

type refHasher func(ref *parser.Ref) (hasher, error)

func (e *Engine) recursivelyComputeHash(target *Target, refHasher refHasher, seen map[string]*parser.Ref, forEach func(*Target, hasher)) (hasher, error) {
	h := newHasher()
	for _, input := range target.inputs.Refs {
		// if orig, ok := seen[input.Text]; ok {
		// 	return 0, participle.Errorf(input.Pos, "circular dependency %s", orig.Pos)
		// }
		inputTarget, err := e.getTarget(input.Text)
		if err != nil {
			return 0, err
		}
		subh, err := e.recursivelyComputeHash(inputTarget, refHasher, seen, forEach)
		if err != nil {
			return 0, err
		}
		h.update(subh)
	}
	for _, output := range target.outputs.Refs {
		seen[output.Text] = output
		subh, err := refHasher(output)
		if err != nil {
			return 0, participle.Wrapf(output.Pos, err, "hash failed")
		}
		h.update(subh)
	}
	forEach(target, h)
	return h, nil
}

// Compute hash of target - inputs and outputs.
func (e *Engine) computeHash(target *Target, refHasher refHasher) (hasher, error) {
	h := newHasher()
	for _, input := range target.inputs.Refs {
		inputTarget, err := e.getTarget(input.Text)
		if err != nil {
			return 0, err
		}
		h.int(uint64(inputTarget.storedHash))
	}
	for _, output := range target.outputs.Refs {
		rh, err := refHasher(output)
		if err != nil {
			return 0, participle.Wrapf(output.Pos, err, "hash failed")
		}
		h.update(rh)
	}
	return h, nil
}

func (e *Engine) dbRefHasher(ref *parser.Ref) (hasher, error) {
	h, ok := e.db.Get(ref.Text)
	if !ok {
		return 0, nil
	}
	return h, nil
}

// Hash real files.
func (e *Engine) realRefHasher(ref *parser.Ref) (hasher, error) {
	h := newHasher()
	info, err := os.Stat(ref.Text)
	if err != nil {
		return 0, err
	}

	h.string(ref.Text)
	h.int(uint64(info.Mode()))
	if !info.IsDir() {
		h.int(uint64(info.Size()))
		h.int(uint64(info.ModTime().UnixNano()))
	}
	return h, nil
}

func (e *Engine) evaluate() error {
	// First pass - expand variables and normalise path references.
	for _, target := range e.targets {
		logger := e.targetLogger(target)
		var outputs []*parser.Ref
		for _, ref := range target.outputs.Refs {
			evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{})
			if err != nil {
				return err
			}

			subRefs, err := parser.ParseRefList(evaluated)
			if err != nil {
				return participle.Errorf(ref.Pos, "failed to parse output %q: %s", evaluated, err)
			}
			for _, subRef := range subRefs.Refs {
				subRef.Pos = ref.Pos
				subRef.Text = e.normalisePath(subRef.Text)
				outputs = append(outputs, subRefs.Refs...)
				key := RefKey(subRef.Text)
				if existing, ok := e.outputs[key]; ok {
					return participle.Errorf(ref.Pos, "duplicate output %q at %s", subRef.Text, existing.pos)
				}
				e.outputs[key] = target
			}
		}
		target.outputs.Refs = outputs
		slices.SortFunc(target.outputs.Refs, func(a, b *parser.Ref) int { return strings.Compare(a.Text, b.Text) })

		// Expand globs.
		var inputs []*parser.Ref
		for _, ref := range target.inputs.Refs {
			evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{})
			if err != nil {
				return err
			}
			innerRefs, err := parser.ParseRefList(evaluated)
			if err != nil {
				return participle.Errorf(ref.Pos, "failed to parse input %q: %s", evaluated, err)
			}
			for _, innerRef := range innerRefs.Refs {
				matches := e.globber.Filepath(innerRef.Text)
				logger.Tracef("Glob %q -> %q", innerRef.Text, strings.Join(matches, " "))
				if len(matches) == 0 {
					matches = []string{innerRef.Text}
				}

				for _, match := range matches {
					inputs = append(inputs,
						&parser.Ref{
							Pos:  ref.Pos,
							Text: match,
						})
					e.inputs[RefKey(match)] = target
				}
			}
		}
		slices.SortFunc(inputs, func(a, b *parser.Ref) int { return strings.Compare(a.Text, b.Text) })
		target.inputs.Refs = inputs
	}

	// Second pass - restore hashes from the DB.
	for _, target := range e.targets {
		logger := e.targetLogger(target)
		_, err := e.recursivelyComputeHash(target, e.dbRefHasher, map[string]*parser.Ref{}, func(target *Target, h hasher) {
			target.storedHash = h
		})
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		_, err = e.recursivelyComputeHash(target, e.realRefHasher, map[string]*parser.Ref{}, func(target *Target, h hasher) {
			target.realHash = h
		})
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		var changed string
		if target.storedHash != target.realHash {
			changed = " (changed)"
		}
		logger.Tracef("hash: %016x > %016x%s", target.storedHash, target.realHash, changed)
	}
	return nil
}

func (e *Engine) targetLogger(target *Target) *Logger {
	return e.log.Scope(strings.Join(target.outputs.Strings(), ":"))
}

func (e *Engine) evaluateString(pos lexer.Position, v string, target *Target, seen map[string]bool) (string, error) {
	text, err := parser.ParseTextString(v)
	if err != nil {
		return "", err
	}

	out := &strings.Builder{}
	for _, fragment := range text.Fragments {
		switch fragment := fragment.(type) {
		case *parser.VarFragment:
			str, err := e.evaluateVar(fragment, target, seen)
			if err != nil {
				var perr participle.Error
				if errors.As(err, &perr) {
					return "", err
				}
				pos = translateVarPos(pos, fragment)
				return "", participle.Errorf(pos, "%s", err)
			}
			out.WriteString(str)

		case *parser.TextFragment:
			out.WriteString(fragment.Text)

		case *parser.CmdFragment:
			cmd, err := e.evaluateString(fragment.Pos, fragment.Cmd, target, seen)
			if err != nil {
				return "", err
			}
			output, err := e.capture(e.log.Scope(strings.Join(target.outputs.Strings(), ":")), cmd)
			if err != nil {
				return "", participle.Errorf(pos, "failed to run command: %s", cmd)
			}
			out.WriteString(output)
		}
	}

	return out.String(), nil
}

func (e *Engine) capture(log *Logger, command string) (string, error) {
	log.Tracef("$ %s", command)
	cmd := exec.Command("sh", "-c", command)
	stdout := &strings.Builder{}
	cmd.Stdout = stdout
	cmd.Stderr = e.log.WriterAt(LogLevelError)
	err := cmd.Run()
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(stdout.String()), nil
}

func (e *Engine) evaluateVar(v *parser.VarFragment, target *Target, seen map[string]bool) (string, error) {
	name := v.Var
	if seen[name] {
		return "", fmt.Errorf("circular variable reference %q", name)
	}
	var block *parser.Block
	var ok bool
	if block, ok = target.vars[name]; !ok {
		if block, ok = e.vars[name]; !ok {
			return "", fmt.Errorf("unknown variable %q", name)
		}
	}
	seen[name] = true
	return e.evaluateString(block.Pos, block.Body, target, seen)
}

func (e *Engine) normalisePath(path string) string {
	if !filepath.IsAbs(path) {
		path = filepath.Clean(filepath.Join(e.cwd, path))
	}
	return strings.TrimPrefix(path, e.cwd+"/")
}

func (e *Engine) getTarget(name string) (*Target, error) {
	name = e.normalisePath(name)
	target, ok := e.outputs[RefKey(name)]
	if ok {
		return target, nil
	}
	_, err := os.Stat(name)
	if err != nil {
		return nil, fmt.Errorf("no such file or target %q", name)
	}
	// Synthetic target.
	target = &Target{
		synthetic: true,
		pos:       lexer.Position{},
		inputs:    &parser.RefList{},
		outputs:   &parser.RefList{Refs: []*parser.Ref{{Text: name}}},
		build: &parser.Command{
			Command: "build",
			Value: &parser.Block{
				Body: "true",
			},
		},
		vars:      Vars{},
		buildFunc: func(logger *Logger, target *Target) error { return nil },
		hashFunc:  func() (hasher, error) { return e.hashFile(name) },
	}
	e.targets = append(e.targets, target)
	e.outputs[RefKey(name)] = target
	return target, nil
}

func (e *Engine) hashFile(name string) (hasher, error) {
	name = e.normalisePath(name)
	info, err := os.Stat(name)
	if errors.Is(err, os.ErrNotExist) {
		return 0, fmt.Errorf("no such file or target %q", name)
	} else if err != nil {
		return 0, err
	}
	h := newHasher()
	h.int(uint64(info.ModTime().UnixNano()))
	return h, nil
}

// Translate fragment position into Bitfile position.
func translateVarPos(pos lexer.Position, fragment *parser.VarFragment) lexer.Position {
	if fragment.Pos.Line != 1 {
		pos.Line += fragment.Pos.Line - 1
		pos.Column = fragment.Pos.Column - 1
	} else {
		pos.Column += fragment.Pos.Column - 1
	}
	pos.Offset += fragment.Pos.Offset
	return pos
}
