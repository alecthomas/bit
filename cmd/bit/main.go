package main

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/alecthomas/kong"
	"github.com/alecthomas/participle/v2"

	"github.com/alecthomas/bit/parser"
)

var cli struct {
	File   *os.File `help:"Bitfile to load." required:"" default:"Bitfile"`
	List   bool     `help:"List available targets."`
	Target string   `arg:"" optional:"" help:"Target to run."`
}

func main() {
	kctx := kong.Parse(&cli)
	defer cli.File.Close()
	_, err := parser.Parse(cli.File.Name(), cli.File)
	if err != nil {
		var perr participle.Error
		if !errors.As(err, &perr) {
			kctx.FatalIfErrorf(err)
		}

		printError(perr)
		kctx.Exit(0)
	}
	kctx.FatalIfErrorf(err)
	// eng, err := engine.Compile(bitfile)
	// kctx.FatalIfErrorf(err)
	// fmt.Println(eng)
}

func printError(perr participle.Error) {
	_, _ = cli.File.Seek(0, 0)
	scanner := bufio.NewScanner(cli.File)
	line := 1
	pos := perr.Position()
	for scanner.Scan() {
		text := scanner.Text()
		if line == pos.Line {
			fmt.Printf("error: %s\n", text)
			break
		}
		line++
	}
	fmt.Printf("%s^ %s\n", strings.Repeat(" ", pos.Column+6), perr.Message())
}
