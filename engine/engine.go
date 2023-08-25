package engine

import (
	"github.com/alecthomas/bit/parser"
)

type Node struct {
	Name      string
	Buildable parser.Buildable
}

type Engine struct {
	virtual map[string]*parser.VirtualTarget
}

func Compile(bitfile *parser.Bitfile) (*Engine, error) {
	panic("??")
}
