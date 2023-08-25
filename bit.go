package bit

import (
	"github.com/alecthomas/bit/parser"
)

type Target interface {
	Hash() [20]byte
}

type Engine struct {
}

func Compile(ast *parser.Bitfile) (*Engine, error) {
	panic("??")
}
