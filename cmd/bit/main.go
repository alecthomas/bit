package main

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"runtime/pprof"
	"strings"

	"github.com/alecthomas/kong"
	"github.com/alecthomas/participle/v2"

	"github.com/alecthomas/bit/engine"
	"github.com/alecthomas/bit/engine/logging"
	"github.com/alecthomas/bit/parser"
)

type CLI struct {
	logging.LogConfig
	CPUProfile string             `help:"Write CPU profile to file." type:"file" hidden:""`
	File       *os.File           `short:"f" help:"Bitfile to load." required:"" default:"Bitfile"`
	Chdir      kong.ChangeDirFlag `short:"C" help:"Change to directory before running." placeholder:"DIR"`
	Timing     bool               `short:"t" help:"Print timing information."`
	Dot        bool               `xor:"command" help:"Print dependency graph as a .dot file."`
	List       bool               `short:"l" xor:"command" help:"List available targets."`
	Describe   string             `short:"D" xor:"command" help:"Describe an aspect of the Bit build. ${describe_help}" required:"" enum:"files,deps,targets,ignored" placeholder:"ASPECT"`
	Clean      bool               `short:"c" xor:"command" help:"Clean targets."`
	Target     []string           `arg:"" optional:"" help:"Target to run."`
}

const description = `
Bit - A simple yet powerful build tool
`

func main() {
	cli := &CLI{}
	kong.Parse(cli, kong.Description(description), kong.HelpOptions{
		FlagsLast: true,
	}, kong.Vars{
		"describe_help": `Where ASPECT is one of:
		files: list all files Bit has determined are inputs and outputs
		deps: show dependency graph
		targets: list all targets
		ignored: list all loaded ignore patterns (from .gitignore files)

`,
	})
	defer cli.File.Close()
	logger := logging.NewLogger(cli.LogConfig)
	bitfile, err := parser.Parse(cli.File.Name(), cli.File)
	reportError(cli.File, logger, err)
	eng, err := engine.Compile(logger, bitfile)
	reportError(cli.File, logger, err)

	if cli.CPUProfile != "" {
		f, err := os.Create(cli.CPUProfile)
		reportError(cli.File, logger, err)
		// The default is 100, but if we're only measuring Bit itself, and not
		// builds, that's too low.
		runtime.SetCPUProfileRate(500)
		_ = pprof.StartCPUProfile(f)
		defer pprof.StopCPUProfile()
	}

	switch {
	case cli.List, cli.Describe == "targets":
		for _, target := range eng.Outputs() {
			fmt.Println(target)
		}

	case cli.Clean:
		err = eng.Clean(cli.Target)
		reportError(cli.File, logger, err)

	case cli.Describe == "deps":
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

	case cli.Describe == "files":
		for _, file := range eng.Files() {
			fmt.Println(file)
		}

	case cli.Describe == "ignored":
		for _, file := range eng.Ignored() {
			fmt.Println(file)
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
		reportError(cli.File, logger, err)
		err = eng.Close()
		reportError(cli.File, logger, err)
	}

}

func reportError(file *os.File, logger *logging.Logger, err error) {
	if err == nil {
		return
	}
	var perr participle.Error
	if !errors.As(err, &perr) {
		logger.Errorf("error: %+v", err)
		os.Exit(1)
	}

	_, _ = file.Seek(0, 0)
	scanner := bufio.NewScanner(file)
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
