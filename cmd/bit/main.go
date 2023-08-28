package main

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/alecthomas/kong"
	"github.com/alecthomas/participle/v2"

	"github.com/alecthomas/bit/engine"
	"github.com/alecthomas/bit/parser"
)

var cli struct {
	engine.LogConfig
	File   *os.File `help:"Bitfile to load." required:"" default:"Bitfile"`
	List   bool     `help:"List available targets."`
	Target string   `arg:"" help:"Target to run."`
}

func main() {
	kctx := kong.Parse(&cli)
	defer cli.File.Close()
	bitfile, err := parser.Parse(cli.File.Name(), cli.File)
	reportError(kctx, err)
	logger := engine.NewLogger(cli.LogConfig)
	eng, err := engine.Compile(logger, bitfile)
	reportError(kctx, err)
	err = eng.Build(cli.Target)
	reportError(kctx, err)
	err = eng.Close()
	reportError(kctx, err)
}

func reportError(kctx *kong.Context, err error) {
	if err == nil {
		return
	}
	var perr participle.Error
	if !errors.As(err, &perr) {
		kctx.FatalIfErrorf(err)
	}

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
	kctx.Exit(1)
}
