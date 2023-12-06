package engine

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
	"github.com/kballard/go-shellquote"
	deepcopy "golang.design/x/reflect"

	"github.com/alecthomas/bit/engine/internal"
	"github.com/alecthomas/bit/engine/logging"
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
	buildFunc func(logger *logging.Logger, target *Target) error
	cleanFunc func(logger *logging.Logger, target *Target) error
	// Hash function for virtual targets.
	hashFunc *internal.MemoisedFunction[Hasher]
	// Hash stored in the DB.
	storedHash Hasher
	// Hash computed from the filesystem.
	realHash  Hasher
	chdir     *parser.Ref
	synthetic bool
}

type RefKey string

type Engine struct {
	cwd     string
	globber *internal.Globber
	log     *logging.Logger
	vars    Vars
	db      *HashDB
	targets []*Target
	outputs map[RefKey]*Target
	inputs  map[RefKey]*Target
}

// Compile a Bitfile into an Engine ready to build targets.
func Compile(logger *logging.Logger, bitfile *parser.Bitfile) (*Engine, error) {
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
	engine := &Engine{
		cwd:     cwd,
		log:     logger,
		db:      db,
		inputs:  map[RefKey]*Target{},
		outputs: map[RefKey]*Target{},
		vars: map[string]*parser.Block{
			"CWD": {Pos: bitfile.Pos, Body: cwd},
		},
	}
	engine.globber, err = internal.NewGlobber(os.DirFS(cwd), engine.Outputs)
	if err != nil {
		return nil, err
	}

	if err := engine.setGlobalVariables(bitfile); err != nil {
		return nil, err
	}
	if err := engine.expandImplicits(bitfile); err != nil {
		return nil, err
	}
	if err := engine.analyse(bitfile); err != nil {
		return nil, err
	}
	if err := engine.evaluate(); err != nil {
		return nil, err
	}
	return engine, nil
}

// Files lists all files that are referenced by the engine.
func (e *Engine) Files() []string {
	return e.globber.Files()
}

// Ignored returns all files that are ignored by the engine.
func (e *Engine) Ignored() []string {
	return e.globber.Ignored()
}

func (e *Engine) analyse(bitfile *parser.Bitfile) error {
	for _, entry := range bitfile.Entries {
		switch entry := entry.(type) {
		case *parser.Target:
			target := &Target{
				vars:      Vars{},
				pos:       entry.Pos,
				inputs:    entry.Inputs,
				outputs:   entry.Outputs,
				cleanFunc: e.defaultCleanFunc,
				buildFunc: e.defaultBuildFunc,
				chdir:     &parser.Ref{Text: "."},
			}
			if entry.Inputs == nil {
				target.inputs = &parser.RefList{Pos: entry.Pos}
			}
			if entry.Outputs == nil {
				fmt.Println(entry.Pos)
				target.outputs = &parser.RefList{Pos: entry.Pos}
			}
			logger := e.targetLogger(target)
			for _, directive := range entry.Directives {
				switch directive := directive.(type) {
				case *parser.Command:
					if directive.Override == parser.OverrideDelete && directive.Value != nil {
						return participle.Errorf(directive.Pos, "delete override cannot have a command body")
					}
					if directive.Override != parser.OverrideDelete && directive.Value == nil {
						return participle.Errorf(directive.Pos, "command is missing command body")
					}
					switch directive.Command {
					case "build":
						target.build = directive

					case "inputs", "outputs":
						refs, err := parser.ParseRefList(directive.Pos, directive.Value.Body)
						if err != nil {
							return participle.Errorf(directive.Value.Pos, "failed to parse %s: %s", directive.Command, err)
						}
						if directive.Command == "inputs" {
							target.inputs.Refs = append(target.inputs.Refs, refs.Refs...)
						} else {
							target.outputs.Refs = append(target.outputs.Refs, refs.Refs...)
						}

					case "hash":
						target.hashPos = directive.Value.Pos
						target.hashFunc = internal.Memoise(func() (Hasher, error) {
							command, err := e.evaluateString(directive.Value.Pos, directive.Value.Body, target, map[string]bool{})
							if err != nil {
								return 0, participle.Errorf(directive.Value.Pos, "hash command is invalid")
							}
							logger.Debugf("$ %s (hashing function)", command)
							cmd := exec.Command("sh", "-c", command)
							output, err := cmd.CombinedOutput()
							if err != nil {
								return 0, participle.Errorf(directive.Value.Pos, "failed to run hash command: %s", err)
							}
							hfh := NewHasher()
							hfh.Bytes(output)
							return hfh, nil
						})

					case "clean":
						if err := e.setTargetCleanFunc(target, directive); err != nil {
							return err
						}

					default:
						return participle.Errorf(directive.Pos, "unsupported command %q", directive.Command)
					}

				case *parser.Chdir:
					target.chdir = directive.Dir

				case *parser.Assignment:
					if directive.Export {
						return participle.Errorf(directive.Pos, "exported variables are not supported on targets yet")
					}
					target.vars[directive.Name] = directive.Value

				default:
					return participle.Errorf(directive.Position(), "unsupported directive %T", directive)
				}
			}
			if target.build == nil {
				return participle.Errorf(entry.Pos, "target has no build command")
			}
			if target.outputs == nil {
				return participle.Errorf(entry.Pos, "target has no outputs")
			}
			e.targets = append(e.targets, target)

		case *parser.Assignment:
			// Done in an earlier phase

		// case *parser.VirtualTarget:
		// case *parser.Template:
		default:
			return participle.Errorf(entry.Position(), "unsupported entry type %T", entry)
		}
	}
	return nil
}

func (e *Engine) setVariable(logger *logging.Logger, entry *parser.Assignment) {
	logger = logger.Scope(entry.Name)
	switch entry.Override {
	case parser.OverrideDelete:
		logger.Debugf("unset %s", entry.Name)
		delete(e.vars, entry.Name)

	case parser.OverrideReplace:
		logger.Debugf("%s=%s", entry.Name, shellquote.Join(entry.Value.Body))
		e.vars[entry.Name] = entry.Value

	case parser.OverrideAppend:
		logger.Debugf("%s=$%s%s", entry.Name, entry.Name, shellquote.Join(entry.Value.Body))
		if _, ok := e.vars[entry.Name]; !ok {
			e.vars[entry.Name] = entry.Value
		} else {
			e.vars[entry.Name].Body += entry.Value.Body
		}

	case parser.OverridePrepend:
		logger.Debugf("%s=%s$%s", entry.Name, shellquote.Join(entry.Value.Body), entry.Name)
		if _, ok := e.vars[entry.Name]; !ok {
			e.vars[entry.Name] = entry.Value
		} else {
			e.vars[entry.Name].Body = entry.Value.Body + e.vars[entry.Name].Body
		}
	}
}

func (e *Engine) exportVariable(logger *logging.Logger, entry *parser.Assignment) error {
	logger = logger.Scope(entry.Name)
	switch entry.Override {
	case parser.OverrideDelete:
		logger.Debugf("unset %s", entry.Name, shellquote.Join(entry.Value.Body))
		if err := os.Unsetenv(entry.Name); err != nil {
			return participle.Wrapf(entry.Pos, err, "failed to unset environment variable %q", entry.Name)
		}

	case parser.OverrideReplace:
		logger.Debugf("export %s=%s", entry.Name, shellquote.Join(entry.Value.Body))
		if err := os.Setenv(entry.Name, entry.Value.Body); err != nil {
			return participle.Wrapf(entry.Pos, err, "failed to set environment variable %q", entry.Name)
		}

	case parser.OverrideAppend:
		logger.Debugf("export %s=$%s%s", entry.Name, entry.Name, shellquote.Join(entry.Value.Body))
		value := os.Getenv(entry.Name) + entry.Value.Body
		if err := os.Setenv(entry.Name, value); err != nil {
			return participle.Wrapf(entry.Pos, err, "failed to set environment variable %q", entry.Name)
		}

	case parser.OverridePrepend:
		logger.Debugf("export %s=%s$%s", entry.Name, shellquote.Join(entry.Value.Body), entry.Name)
		value := entry.Value.Body + os.Getenv(entry.Name)
		if err := os.Setenv(entry.Name, value); err != nil {
			return participle.Wrapf(entry.Pos, err, "failed to set environment variable %q", entry.Name)
		}

	}
	return nil
}

func (e *Engine) setTargetCleanFunc(target *Target, directive *parser.Command) error {
	switch directive.Override {
	case parser.OverrideDelete:
		target.cleanFunc = func(logger *logging.Logger, target *Target) error { return nil }

	case parser.OverrideReplace:
		target.cleanFunc = func(logger *logging.Logger, target *Target) error {
			return logger.Exec(target.chdir.Text, directive.Value.Body)
		}

	case parser.OverrideAppend:
		cleanFunc := target.cleanFunc
		target.cleanFunc = func(logger *logging.Logger, target *Target) error {
			if err := cleanFunc(logger, target); err != nil {
				return err
			}
			return logger.Exec(target.chdir.Text, directive.Value.Body)
		}

	case parser.OverridePrepend:
		cleanFunc := target.cleanFunc
		target.cleanFunc = func(logger *logging.Logger, target *Target) error {
			if err := logger.Exec(target.chdir.Text, directive.Value.Body); err != nil {
				return err
			}
			return cleanFunc(logger, target)
		}

	default:
		return participle.Errorf(directive.Pos, "clean does not support the %s override", directive.Override)
	}
	return nil
}

func (e *Engine) Clean(outputs []string) error {
	outputs, err := e.expandOutputs(outputs)
	if err != nil {
		return err
	}
	outputsSet := map[string]bool{}
	for _, output := range outputs {
		outputsSet[output] = true
	}
nextTarget:
	for _, target := range e.targets {
		if target.synthetic {
			continue
		}
		if len(outputsSet) > 0 {
			for _, output := range target.outputs.Refs {
				if !outputsSet[output.Text] {
					continue nextTarget
				}
				break
			}
		}
		logger := e.targetLogger(target)
		err := target.cleanFunc(logger, target)
		if err != nil {
			return participle.Wrapf(target.build.Pos, err, "clean failed")
		}
	}
	return nil
}

func (e *Engine) Outputs() []string {
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
			h, err := e.realRefHasher(target, output)
			if err != nil {
				continue
			}
			e.db.Set(output.Text, h)
		}
	}
	return e.db.Close()
}

func (e *Engine) Build(outputs []string) error {
	outputs, err := e.expandOutputs(outputs)
	if err != nil {
		return err
	}
	return e.build(outputs, map[string]bool{})
}

// Glob-expand outputs.
func (e *Engine) expandOutputs(outputs []string) ([]string, error) {
	expanded := []string{}
	for _, output := range outputs {
		normalised, err := e.normalisePath(output)
		if err != nil {
			return nil, err
		}
		globbed := e.globber.MatchFilesystem(normalised)
		if len(globbed) == 0 {
			return nil, fmt.Errorf("no matching outputs for %q", normalised)
		}
		expanded = append(expanded, globbed...)
	}
	return expanded, nil
}

func (e *Engine) build(outputs []string, seen map[string]bool) error {
	if len(outputs) == 0 {
		outputs = e.Outputs()
	}
	for _, name := range outputs {
		name, err := e.normalisePath(name) //nolint:govet
		if err != nil {
			return err
		}
		if seen[name] {
			continue
		}
		seen[name] = true

		log := e.log.Scope(name)

		target, err := e.getTarget(name)
		if err != nil {
			return err
		}

		if target.storedHash == target.realHash {
			if !target.synthetic {
				log.Debugf("Up to date.")
			}
			continue
		}

		// Build dependencies.
		for _, input := range target.inputs.Refs {
			if err := e.build([]string{input.Text}, seen); err != nil {
				return participle.Wrapf(input.Pos, err, "build failed")
			}
		}

		log.Tracef("Building.")

		// Build target.
		err = target.buildFunc(log, target)
		if err != nil {
			return participle.Wrapf(target.build.Pos, err, "build failed")
		}

		h, err := e.computeHash(target, e.realRefHasher)
		if err != nil {
			if errors.Is(err, os.ErrNotExist) {
				var perr participle.Error
				if errors.As(err, &perr) {
					return participle.Errorf(perr.Position(), "target did not produce expected output: %s", perr.Message())
				}
				return fmt.Errorf("target did not produce expected output: %w", err)
			}
			return err
		}
		target.storedHash = h
		target.realHash = h
	}
	return nil
}

func (e *Engine) defaultBuildFunc(log *logging.Logger, target *Target) error {
	block := target.build.Value
	command, err := e.evaluateString(block.Pos, block.Body, target, map[string]bool{})
	if err != nil {
		return participle.Wrapf(block.Pos, err, "invalid command %q", command)
	}
	err = log.Exec(target.chdir.Text, command)
	if err != nil {
		return participle.Wrapf(block.Pos, err, "failed to run command %q", command)
	}
	return nil
}

// A function used to compute a hash of an output.
type outputRefHasher func(target *Target, ref *parser.Ref) (Hasher, error)

func (e *Engine) recursivelyComputeHash(target *Target, refHasher outputRefHasher, seen map[string]bool, forEach func(*Target, Hasher)) (Hasher, error) {
	h := NewHasher()
	for _, input := range target.inputs.Refs {
		if _, ok := seen[input.Text]; ok {
			continue
		}
		seen[input.Text] = true
		inputTarget, err := e.getTarget(input.Text)
		if err != nil {
			return 0, participle.Wrapf(input.Pos, err, "couldn't find matching input")
		}
		subh, err := e.recursivelyComputeHash(inputTarget, refHasher, seen, forEach)
		if err != nil {
			return 0, err
		}
		h.Update(subh)
	}
	for _, output := range target.outputs.Refs {
		rh, err := refHasher(target, output)
		if err != nil {
			return 0, participle.Wrapf(output.Pos, err, "hash failed")
		}
		h.Update(rh)
	}
	forEach(target, h)
	return h, nil
}

// Compute hash of target - inputs and outputs.
func (e *Engine) computeHash(target *Target, refHasher outputRefHasher) (Hasher, error) {
	h := NewHasher()
	for _, input := range target.inputs.Refs {
		inputTarget, err := e.getTarget(input.Text)
		if err != nil {
			return 0, err
		}
		h.Int(uint64(inputTarget.storedHash))
	}
	for _, output := range target.outputs.Refs {
		rh, err := refHasher(target, output)
		if err != nil {
			return 0, participle.Wrapf(output.Pos, err, "hash failed")
		}
		h.Update(rh)
	}
	return h, nil
}

func (e *Engine) dbRefHasher(target *Target, ref *parser.Ref) (Hasher, error) { //nolint:revive
	h, ok := e.db.Get(ref.Text)
	if !ok {
		return 0, nil
	}
	return h, nil
}

// Hash real files.
func (e *Engine) realRefHasher(target *Target, ref *parser.Ref) (Hasher, error) {
	h := NewHasher()
	h.Str(ref.Text)

	// If we have a hash function, use that for every reference.
	if target.hashFunc != nil {
		hf, err := target.hashFunc.Get()
		if err != nil {
			return 0, participle.Errorf(target.hashPos, "failed to compute hash: %s", err)
		}
		h.Update(hf)
		return h, nil
	}

	info, err := os.Stat(ref.Text)
	if err != nil {
		return 0, err
	}

	h.Int(uint64(info.Mode()))
	if !info.IsDir() {
		h.Int(uint64(info.Size()))
		h.Int(uint64(info.ModTime().UnixNano()))
	}
	return h, nil
}

// Expand variables and normalise path references.
func (e *Engine) evaluate() error {
	collectedOutputs := map[string]bool{}

	// Evaluate outputs first so that inputs can potentially match against them.
	for _, target := range e.targets {
		var outputs []*parser.Ref
		for _, ref := range target.outputs.Refs {
			evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{})
			if err != nil {
				return err
			}

			subRefs, err := parser.ParseRefList(ref.Pos, evaluated)
			if err != nil {
				return participle.Errorf(ref.Pos, "failed to parse output %q: %s", evaluated, err)
			}
			for _, subRef := range subRefs.Refs {
				subRef.Text, err = e.normalisePath(subRef.Text)
				if err != nil {
					return participle.Errorf(subRef.Pos, "%s", err)
				}
				// TODO: Supporting brace expansion would be very useful here.
				if e.globber.IsGlob(subRef.Text) {
					return participle.Errorf(ref.Pos, "globs are not allowed in output (%s)", subRef.Text)
				}
				outputs = append(outputs, subRefs.Refs...)
				key := RefKey(subRef.Text)
				if existing, ok := e.outputs[key]; ok {
					return participle.Errorf(ref.Pos, "duplicate output %q at %s", subRef.Text, existing.pos)
				}
				e.outputs[key] = target
				collectedOutputs[subRef.Text] = true
			}
		}
		target.outputs.Refs = outputs
		slices.SortFunc(target.outputs.Refs, func(a, b *parser.Ref) int { return strings.Compare(a.Text, b.Text) })

		evaluated, err := e.evaluateString(target.chdir.Pos, target.chdir.Text, target, map[string]bool{})
		if err != nil {
			return err
		}
		target.chdir.Text, err = e.normalisePath(evaluated)
		if err != nil {
			return participle.Errorf(target.chdir.Pos, "%s", err)
		}
	}

	for _, target := range e.targets {
		logger := e.targetLogger(target)

		// Expand input globs.
		collectedInputs := map[string]bool{}
		var inputs []*parser.Ref
		for _, ref := range target.inputs.Refs {
			evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{})
			if err != nil {
				return err
			}
			innerRefs, err := parser.ParseRefList(ref.Pos, evaluated)
			if err != nil {
				return participle.Errorf(ref.Pos, "failed to parse input %q: %s", evaluated, err)
			}
			for _, innerRef := range innerRefs.Refs {
				matches := e.globber.MatchFilesystem(innerRef.Text)
				logger.Tracef("Glob %s -> %s", innerRef.Text, strings.Join(matches, " "))
				if len(matches) == 0 {
					matches = []string{innerRef.Text}
				}

				for _, match := range matches {
					inputs = append(inputs, &parser.Ref{Pos: ref.Pos, Text: match})
					e.inputs[RefKey(match)] = target
					collectedInputs[match] = true
				}
			}
		}
		slices.SortFunc(inputs, func(a, b *parser.Ref) int { return strings.Compare(a.Text, b.Text) })
		target.inputs.Refs = inputs

		target.vars["IN"] = &parser.Block{
			Pos:  target.inputs.Pos,
			Body: strings.Join(target.inputs.Strings(), " "),
		}
		target.vars["OUT"] = &parser.Block{
			Pos:  target.outputs.Pos,
			Body: strings.Join(target.outputs.Strings(), " "),
		}
	}

	// Second pass - restore hashes from the DB.
	for _, target := range e.targets {
		logger := e.targetLogger(target)
		_, err := e.recursivelyComputeHash(target, e.dbRefHasher, map[string]bool{}, func(target *Target, h Hasher) {
			target.storedHash = h
		})
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		_, err = e.recursivelyComputeHash(target, e.realRefHasher, map[string]bool{}, func(target *Target, h Hasher) {
			target.realHash = h
		})
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		var changed string
		if target.storedHash != target.realHash {
			changed = " (changed)"
		}
		logger.Tracef("Hash: %016x -> %016x%s", target.storedHash, target.realHash, changed)
	}
	return nil
}

func (e *Engine) targetLogger(target *Target) *logging.Logger {
	return e.log.Scope(strings.Join(target.outputs.Strings(), ":"))
}

// Evaluate variables and commands in a string.
func (e *Engine) evaluateString(pos lexer.Position, v string, target *Target, seen map[string]bool) (string, error) {
	text, err := parser.ParseTextString(v)
	if err != nil {
		var perr participle.Error
		if errors.As(err, &perr) {
			return "", participle.Errorf(translateVarPos(pos, perr.Position()), "%s", perr.Message())
		}
		return "", participle.Errorf(pos, "%s", err)
	}

	out := &strings.Builder{}
	for _, fragment := range text.Fragments {
		switch fragment := fragment.(type) {
		case *parser.VarFragment:
			str, err := e.evaluateVar(fragment, target, seen)
			if err != nil {
				return "", err
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

func (e *Engine) capture(log *logging.Logger, command string) (string, error) {
	log.Tracef("$ %s", command)
	cmd := exec.Command("sh", "-c", command)
	stdout := &strings.Builder{}
	cmd.Stdout = stdout
	cmd.Stderr = e.log.WriterAt(logging.LogLevelError)
	err := cmd.Run()
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(stdout.String()), nil
}

// Evaluate a variable. "target" may be nil.
func (e *Engine) evaluateVar(v *parser.VarFragment, target *Target, seen map[string]bool) (string, error) {
	name := v.Var
	if seen[name] {
		return "", fmt.Errorf("circular variable reference %q", name)
	}
	seen[name] = true
	defer delete(seen, name)

	var block *parser.Block
	var ok bool
	if target != nil {
		block, ok = target.vars[name]
	}
	if !ok {
		if block, ok = e.vars[name]; !ok {
			return "", fmt.Errorf("unknown variable %q", name)
		}
	}
	return e.evaluateString(block.Pos, block.Body, target, seen)
}

func (e *Engine) normalisePath(path string) (string, error) {
	if !filepath.IsAbs(path) {
		path = filepath.Clean(filepath.Join(e.cwd, path))
	}
	if path == e.cwd {
		return ".", nil
	}
	path = strings.TrimPrefix(path, e.cwd+"/")
	if !filepath.IsLocal(path) {
		return "", fmt.Errorf("path %q is not local", path)
	}
	return path, nil
}

func (e *Engine) getTarget(name string) (*Target, error) {
	var err error
	name, err = e.normalisePath(name)
	if err != nil {
		return nil, err
	}
	target, ok := e.outputs[RefKey(name)]
	if ok {
		return target, nil
	}
	_, err = os.Stat(name)
	if err != nil {
		return nil, fmt.Errorf("no such file or target %q", name)
	}
	// Synthetic target.
	target = &Target{
		synthetic: true,
		inputs:    &parser.RefList{},
		outputs:   &parser.RefList{Refs: []*parser.Ref{{Text: name}}},
		build: &parser.Command{
			Command: "build",
			Value: &parser.Block{
				Body: "true",
			},
		},
		vars:      Vars{},
		cleanFunc: e.defaultCleanFunc,
		buildFunc: func(logger *logging.Logger, target *Target) error { return nil },
		chdir:     &parser.Ref{Text: "."},
	}
	e.targets = append(e.targets, target)
	e.outputs[RefKey(name)] = target
	return target, nil
}

func (e *Engine) defaultCleanFunc(logger *logging.Logger, target *Target) error {
	seen := map[string]bool{}
	var remove []string
	for _, output := range target.outputs.Strings() {
		if seen[output] {
			continue
		}
		seen[output] = true
		remove = append(remove, output)

	}
	logger.Noticef("$ rm -rf %s", shellquote.Join(remove...))
	for _, output := range remove {
		err := os.RemoveAll(output)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return participle.Wrapf(target.pos, err, "failed to remove %q", output)
		}
	}
	return nil
}

func (e *Engine) Deps() map[string][]string {
	deps := map[string][]string{}
	for _, target := range e.targets {
		for _, output := range target.outputs.Refs {
			inputs := target.inputs.Strings()
			if len(inputs) == 0 {
				continue
			}
			deps[output.Text] = inputs
		}
	}
	return deps
}

func (e *Engine) expandImplicits(bitfile *parser.Bitfile) error {
	outEntries := make([]parser.Entry, 0, len(bitfile.Entries))
	for _, entry := range bitfile.Entries {
		implicit, ok := entry.(*parser.ImplicitTarget)
		if !ok {
			outEntries = append(outEntries, entry)
			continue
		}
		evaluated, err := e.evaluateString(implicit.Pattern.Pos, implicit.Pattern.Text, nil, map[string]bool{})
		if err != nil {
			return err
		}

		glob, err := e.normalisePath(evaluated)
		if err != nil {
			return participle.Errorf(implicit.Pattern.Pos, "%s", err)
		}
		logger := e.log.Scope(glob)
		logger.Tracef("Expanding implicit target")

		if strings.ContainsAny(glob, "[]") {
			return participle.Errorf(implicit.Pattern.Pos, "implicit pattern glob does not currently support [], {} or ?")
		}

		re := e.globToRe(glob)
		logger.Tracef("Implicit pattern: %s -> %s", glob, re)

		count := 0
		// Create a new target for each file that matches the pattern.
		for _, file := range e.globber.MatchFilesystem(glob) {
			matches := re.FindStringSubmatch(file)
			count++
			logger.Tracef("Matched %s", file)
			target := &parser.Target{
				Pos:        implicit.Pos,
				Docs:       implicit.Docs,
				Directives: deepcopy.DeepCopy(implicit.Directives),
				Inputs:     &parser.RefList{Pos: implicit.Pattern.Pos, Refs: []*parser.Ref{deepcopy.DeepCopy(implicit.Pattern)}},
				Outputs:    &parser.RefList{Pos: implicit.Replace.Pos, Refs: []*parser.Ref{deepcopy.DeepCopy(implicit.Replace)}},
			}
			target.Inputs.Refs[0].Text = file
			target.Directives = append(target.Directives, &parser.Assignment{
				Pos:  implicit.Pos,
				Name: "IN",
				Value: &parser.Block{
					Pos:  implicit.Pattern.Pos,
					Body: file,
				},
			}, &parser.Assignment{
				Pos:  implicit.Pos,
				Name: "OUT",
				Value: &parser.Block{
					Pos:  implicit.Replace.Pos,
					Body: implicit.Replace.Text,
				},
			})
			for i, match := range matches {
				target.Directives = append(target.Directives, &parser.Assignment{
					Pos:  implicit.Pos,
					Name: strconv.Itoa(i),
					Value: &parser.Block{
						Pos:  implicit.Pattern.Pos,
						Body: match,
					},
				})
			}
			outEntries = append(outEntries, target)
		}
		logger.Tracef("Matched %d files", count)
	}
	bitfile.Entries = outEntries
	return nil
}

var globSplitter = regexp.MustCompile(`\*\*/|\*|[^[?*{]+|{[^}]+}|\?`)

func (e *Engine) globToRe(glob string) *regexp.Regexp {
	pattern := ""
	parts := globSplitter.FindAllString(glob, -1)
	for _, part := range parts {
		switch {
		case part == "?":
			pattern += "."

		case part == "*":
			pattern += "([^/]*)"

		case part == "**/":
			pattern += "(.*?/?)?"

		case strings.HasPrefix(part, "{") && strings.HasSuffix(part, "}"):
			pattern += "(" + strings.Join(strings.Split(part[1:len(part)-1], ","), "|") + ")"

		default:
			pattern += regexp.QuoteMeta(part)
		}
	}
	re := regexp.MustCompile("^" + pattern + "$")
	return re
}

// Evaluate global variables into the engine.
func (e *Engine) setGlobalVariables(bitfile *parser.Bitfile) error {
	for _, entry := range bitfile.Entries {
		entry, ok := entry.(*parser.Assignment)
		if !ok {
			continue
		}
		if entry.Export {
			if err := e.exportVariable(e.log, entry); err != nil {
				return err
			}
		} else {
			e.setVariable(e.log, entry)
		}
	}
	return nil
}

// Translate fragment position into Bitfile position.
func translateVarPos(parent lexer.Position, pos lexer.Position) lexer.Position {
	if pos.Line != 1 {
		parent.Line += pos.Line - 1
		parent.Column = pos.Column - 1
	} else {
		parent.Column += pos.Column - 1
	}
	parent.Offset += pos.Offset
	return parent
}
