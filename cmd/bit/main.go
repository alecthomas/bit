package main

import (
	"fmt"

	"github.com/alecthomas/kong"

	"github.com/alecthomas/bit/parser"
)

var cli struct {
}

func main() {
	kong.Parse(&cli)
	fmt.Println(parser.EBNF())
}
