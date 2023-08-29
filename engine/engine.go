package engine

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
	"github.com/kballard/go-shellquote"

	"github.com/alecthomas/bit/parser"
)

type Vars map[string]*parser.Block

type Target struct {
	pos     lexer.Position
	inputs  *parser.RefList
	outputs *parser.RefList
	build   *parser.Command
	hashPos lexer.Position
	vars    Vars
	// If present, the target has been built and this is its hash.
	hash hash
}

type RefKey string

type Engine struct {
	cwd     string
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
	hash := sha256.Sum256([]byte(dir))
	db, err := NewHashDB(filepath.Join(dir, fmt.Sprintf("%x.json", hash)))
	if err != nil {
		return nil, err
	}
	engine := &Engine{
		cwd:     cwd,
		log:     logger,
		db:      db,
		inputs:  map[RefKey]*Target{},
		outputs: map[RefKey]*Target{},
		vars:    map[string]*parser.Block{},
	}
	for _, entry := range bitfile.Entries {
		switch entry := entry.(type) {
		case *parser.Target:
			target := &Target{
				pos:     entry.Pos,
				inputs:  entry.Inputs,
				outputs: entry.Outputs,
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
			engine.targets = append(engine.targets, target)

		case *parser.Assignment:
			engine.vars[entry.Name] = entry.Value

		// case *parser.VirtualTarget:
		// case *parser.Template:
		default:
			panic(fmt.Sprintf("unsupported entry type %T", entry))
		}
	}

	if err := engine.evaluate(); err != nil {
		return nil, err
	}
	return engine, nil
}

func (e *Engine) Close() error {
	for _, target := range e.targets {
		for _, output := range target.outputs.Refs {
			h, err := e.realRefHasher(output)
			if err != nil {
				continue
			}
			e.db.Delete(output.Text)
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

	if target.hash != 0 {
		h, err := e.recomputeHash(target, e.realRefHasher)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		if h == target.hash {
			log.Tracef("Skipping %s, up to date.", name)
			return nil
		}
	}

	// Build dependencies.
	for _, input := range target.inputs.Refs {
		if err := e.Build(input.Text); err != nil {
			return participle.Wrapf(input.Pos, err, "build failed")
		}
	}

	block := target.build.Value
	command, err := e.evaluateString(block.Pos, block.Body, target, map[string]bool{})
	if err != nil {
		return participle.Wrapf(block.Pos, err, "invalid command %q", command)
	}
	args, err := shellquote.Split(command)
	if err != nil {
		return participle.Wrapf(block.Pos, err, "invalid command %q", command)
	}
	err = log.Exec(args...)
	if err != nil {
		return participle.Wrapf(block.Pos, err, "failed to run command %q", command)
	}
	h, err := e.recomputeHash(target, e.realRefHasher)
	if err != nil {
		return err
	}
	target.hash = h
	return nil
}

type refHasher func(ref *parser.Ref) (hash, error)

// Recompute hash of target.
func (e *Engine) recomputeHash(target *Target, refHasher refHasher) (hash, error) {
	h := newHasher()
	for _, input := range target.inputs.Refs {
		inputTarget, err := e.getTarget(input.Text)
		if err != nil {
			return 0, err
		}
		h = h.int(uint64(inputTarget.hash))
	}
	for _, output := range target.outputs.Refs {
		rh, err := refHasher(output)
		if err != nil {
			return 0, participle.Wrapf(output.Pos, err, "hash failed")
		}
		h = h.update(rh)
	}
	return h, nil
}

func (e *Engine) dbRefHasher(ref *parser.Ref) (hash, error) {
	h, ok := e.db.Get(ref.Text)
	if !ok {
		return 0, nil
	}
	return h, nil
}

// Hash real files.
func (e *Engine) realRefHasher(ref *parser.Ref) (hash, error) {
	h := newHasher()
	info, err := os.Stat(ref.Text)
	if err != nil {
		return 0, err
	}

	h = h.string(ref.Text)
	h = h.int(uint64(info.Mode()))
	if !info.IsDir() {
		h = h.int(uint64(info.Size()))
		h = h.int(uint64(info.ModTime().UnixNano()))
	}
	return h, nil
}

func (e *Engine) evaluate() error {
	for _, target := range e.targets {
		for _, ref := range target.outputs.Refs {
			if evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{}); err != nil {
				return err
			} else {
				evaluated = e.normalisePath(evaluated)
				ref.Text = evaluated
				key := RefKey(evaluated)
				if existing, ok := e.outputs[key]; ok {
					return participle.Errorf(ref.Pos, "duplicate output %q at %s", ref.Text, existing.pos)
				}
				e.outputs[key] = target
			}
		}
		slices.SortFunc(target.outputs.Refs, func(a, b *parser.Ref) int { return strings.Compare(a.Text, b.Text) })

		for _, ref := range target.inputs.Refs {
			if evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{}); err != nil {
				return err
			} else {
				evaluated = e.normalisePath(evaluated)
				ref.Text = evaluated
				e.inputs[RefKey(evaluated)] = target
			}
		}
		slices.SortFunc(target.inputs.Refs, func(a, b *parser.Ref) int { return strings.Compare(a.Text, b.Text) })
	}

	for _, target := range e.targets {
		h, err := e.recomputeHash(target, e.dbRefHasher)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		target.hash = h
	}
	return nil
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
		}
	}

	return out.String(), nil
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
		pos:     lexer.Position{},
		inputs:  &parser.RefList{},
		outputs: &parser.RefList{Refs: []*parser.Ref{{Text: name}}},
		build: &parser.Command{
			Command: "build",
			Value: &parser.Block{
				Body: "true",
			},
		},
		vars: Vars{},
	}
	h, err := e.recomputeHash(target, e.dbRefHasher)
	if err != nil {
		return nil, err
	}
	target.hash = h
	e.targets = append(e.targets, target)
	e.outputs[RefKey(name)] = target
	return target, nil
}

func (e *Engine) hashFile(name string) (hash, error) {
	name = e.normalisePath(name)
	info, err := os.Stat(name)
	if errors.Is(err, os.ErrNotExist) {
		return 0, fmt.Errorf("no such file or target %q", name)
	} else if err != nil {
		return 0, err
	}
	h := newHasher().int(uint64(info.ModTime().UnixNano()))
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

// fnv64a hash function.
const offset64 = 14695981039346656037
const prime64 = 1099511628211

type hash uint64

func newHasher() hash { return offset64 }

func (f hash) int(data uint64) hash {
	f ^= hash(data)
	f *= prime64
	return f
}

func (f hash) update(h hash) hash {
	f ^= h
	f *= prime64
	return f
}

func (f hash) string(data string) hash {
	for _, c := range data {
		f ^= hash(c)
		f *= prime64
	}
	return f
}

func (f hash) bytes(data []byte) hash {
	for _, c := range data {
		f ^= hash(c)
		f *= prime64
	}
	return f
}
