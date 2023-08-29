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
	File   *os.File `help:"Bitfile to load." required:"" default:"Bitfile"`
	List   bool     `help:"List available targets."`
	Target string   `arg:"" help:"Target to run."`
}

func main() {
	kong.Parse(&cli)
	defer cli.File.Close()
	logger := engine.NewLogger(cli.LogConfig)
	bitfile, err := parser.Parse(cli.File.Name(), cli.File)
	reportError(logger, err)
	eng, err := engine.Compile(logger, bitfile)
	reportError(logger, err)
	err = eng.Build(cli.Target)
	reportError(logger, err)
	err = eng.Close()
	reportError(logger, err)
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
