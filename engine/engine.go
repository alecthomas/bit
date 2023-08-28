package engine

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"hash/fnv"
	"os"
	"path/filepath"
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
	hash    func(list *parser.RefList) (uint64, error)
	vars    Vars
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

// Compile a Bitfile into an Engine.
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
				pos: entry.Pos,
			}
			if entry.Inputs != nil {
				target.inputs = entry.Inputs
			}
			if entry.Outputs != nil {
				target.outputs = entry.Outputs
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
	return e.db.Close()
}

func (e *Engine) Build(name string) error {
	name = e.normalisePath(name)
	log := e.log.Scope(name)
	target, ok := e.outputs[RefKey(name)]
	if !ok {
		return fmt.Errorf("unknown target %q", name)
	}
	// Check if we need to build.
	if h, err := e.hashRef(target, name); err == nil {
		if stored, ok := e.db.Get(name); ok && stored == h {
			log.Noticef("Nothing to do.")
			return nil
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
	err = e.writeHahes(log, target)
	if err != nil {
		return err
	}
	return nil
}

func (e *Engine) writeHahes(log *Logger, target *Target) error {
	for _, ref := range target.outputs.Refs {
		h, err := e.hashRef(target, ref.Text)
		if err != nil {
			return participle.Errorf(target.pos, "%s", err)
		}
		log.Tracef("hash = %d", h)
		e.db.Set(e.normalisePath(ref.Text), h)
	}
	return nil
}

func (e *Engine) hashRef(target *Target, ref string) (uint64, error) {
	info, err := os.Stat(ref)
	if err != nil {
		return 0, participle.Errorf(target.pos, "did not generate output %q", ref)
	}
	h := fnv.New64a()
	h.Write([]byte(fmt.Sprintf("%d", info.ModTime().UnixNano())))
	return h.Sum64(), nil
}

func (e *Engine) evaluate() error {
	for _, target := range e.targets {
		for _, ref := range target.outputs.Refs {
			if evaluated, err := e.evaluateString(ref.Pos, ref.Text, target, map[string]bool{}); err != nil {
				return err
			} else {
				ref.Text = evaluated
				evaluated = e.normalisePath(evaluated)
				e.outputs[RefKey(evaluated)] = target
			}
		}
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
