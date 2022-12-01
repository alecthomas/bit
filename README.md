# A zero-configuration build tool powered by Ninja
[![CI](https://github.com/alecthomas/bit/actions/workflows/ci.yml/badge.svg)](https://github.com/alecthomas/bit/actions/workflows/ci.yml)

Bit is a build tool that requires (close to) zero-configuration. To achieve this
it relies on "analysers" to automatically extract dependencies and construct
Ninja build rules from source files.

For example, here's what the generated `build.ninja` file for Bit itself looks
like:

```
$ bit
dest = build
rule gocmd
  command = go build -trimpath -buildvcs=false -ldflags="-s -w -buildid=" -o $out ./$dir
build $dest/bit: gocmd cmd/bit/main.go golang/enum_enumer.go golang/generate.go golang/cmd.go golang/common.go
  dir = cmd/bit
rule go-generate-enumer-426fc95dec889c44
  command = cd $dir && enumer -type=Enum -json -text
build golang/enum_enumer.go: go-generate-enumer-426fc95dec889c44 golang/generate.go
  dir = golang
```

Bit generated this by recursively collecting all files and matching them
against a set of analysers. In this case the "Go command" analyser matched the
file `cmd/bit/main.go`. The analyser then parsed the file, checking it was
indeed a `main` package, before recursively parsing its imports to collect all
dependent source files. The second analyser to match is one that translates the
commands in `//go:generate` directives into Ninja rules.