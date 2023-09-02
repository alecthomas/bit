package main

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/alecthomas/kong"
	"github.com/alecthomas/participle/v2"

	"github.com/alecthomas/bit/engine"
	"github.com/alecthomas/bit/parser"
)

var cli struct {
	engine.LogConfig
	File   *os.File           `short:"f" help:"Bitfile to load." required:"" default:"Bitfile"`
	Chdir  kong.ChangeDirFlag `short:"C" help:"Change to directory before running." placeholder:"DIR"`
	Deps   bool               `xor:"command" help:"Print dependency graph in a make-compatible format."`
	Dot    bool               `xor:"command" help:"Print dependency graph as a .dot file."`
	List   bool               `short:"l" xor:"command" help:"List available targets."`
	Clean  bool               `short:"c" xor:"command" help:"Clean targets."`
	Target []string           `arg:"" optional:"" help:"Target to run."`
}

func main() {
	kong.Parse(&cli)
	defer cli.File.Close()
	logger := engine.NewLogger(cli.LogConfig)
	bitfile, err := parser.Parse(cli.File.Name(), cli.File)
	reportError(logger, err)
	eng, err := engine.Compile(logger, bitfile)
	reportError(logger, err)

	switch {
	case cli.List:
		for _, target := range eng.Outputs() {
			fmt.Println(target)
		}

	case cli.Clean:
		err = eng.Clean()
		reportError(logger, err)

	case cli.Deps:
		deps := eng.Deps()
		for in, deps := range deps {
			w := len(in) + 1
			fmt.Printf("%s:", in)
			for _, dep := range deps {
				if w+len(dep) > 80 {
					fmt.Printf(" \\\n\t")
					w = 8
				}
				w += len(dep)
				fmt.Printf(" %s", dep)
			}
			fmt.Println()
		}

	case cli.Dot:
		fmt.Println("digraph {")
		for in, deps := range eng.Deps() {
			for _, dep := range deps {
				fmt.Printf("\t%q -> %q;\n", in, dep)
			}
		}
		fmt.Println("}")

	default:
		err = eng.Build(cli.Target)
		reportError(logger, err)
		err = eng.Close()
		reportError(logger, err)
	}

}

func reportError(logger *engine.Logger, err error) {
	if err == nil {
		return
	}
	var perr participle.Error
	if !errors.As(err, &perr) {
		logger.Errorf("error: %+v", err)
		os.Exit(1)
	}

	_, _ = cli.File.Seek(0, 0)
	scanner := bufio.NewScanner(cli.File)
	line := 1
	pos := perr.Position()
	prefix := fmt.Sprintf("%s:%d:%d: ", filepath.Base(pos.Filename), pos.Line, pos.Column)
	for scanner.Scan() {
		text := scanner.Text()
		if line == pos.Line {
			logger.Infof("%s%s", prefix, text)
			break
		}
		line++
	}
	if len(prefix)+len(perr.Message()) > 80 {
		logger.Errorf("%s^\n    error: %s", strings.Repeat(" ", pos.Column+len(prefix)-1), perr.Message())
	} else {
		logger.Errorf("%s^ error: %s", strings.Repeat(" ", pos.Column+len(prefix)-1), perr.Message())
	}
	os.Exit(1)
}
