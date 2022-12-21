package main

import (
	"os"

	"github.com/alecthomas/bit"
	"github.com/alecthomas/bit/golang"
	"github.com/alecthomas/kong"
)

var cli struct {
	DryRun bool   `short:"n" help:"Dry run. Don't actually run any commands."`
	Chdir  string `short:"C" help:"Change to directory before running." default:"."`
	Dest   string `short:"d" help:"Destination directory for generated artefacts." default:"build"`
}

func main() {
	kctx := kong.Parse(&cli,
		kong.Description(`A zero-configuration build tool powered by Ninja`),
	)
	if cli.Chdir != "" {
		if err := os.Chdir(cli.Chdir); err != nil {
			kctx.FatalIfErrorf(err)
		}
	}
	err := bit.Build([]bit.Analyser{
		golang.NewCmdAnalyser(),
		golang.NewGenerateAnalyser(),
	}, bit.DryRun(cli.DryRun), bit.WithDest(cli.Dest))
	kctx.FatalIfErrorf(err)
}
