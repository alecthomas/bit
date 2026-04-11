# bit — Build It

A declarative build tool with dependency tracking, content-based caching, and parallel execution.

bit reads a `BUILD.bit` file, resolves dependencies between blocks, detects what changed, and only rebuilds what's needed. Language-aware providers (e.g. Go, Docker) automatically discover inputs from source files, so in most cases you don't need to specify them manually.

Like Terraform, bit tracks the state of each block between runs. It detects drift (e.g. a deleted Docker image or stopped container), determines what actions are needed (create, update, replace, destroy), and applies only the minimum changes. `bit plan` shows what would change; `bit apply` makes it so; `bit destroy` tears it down.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/alecthomas/bit/main/install.sh | sh
```

Or from source:

```sh
cargo install --path .
```

## Quick Start

```bit
# Go inputs are auto-detected from source files
server = go.exe {
  package = "./cmd/server"
  output = "dist/server"
}

server-linux = go.exe {
  package = "./cmd/server"
  output = "dist/server-linux-arm64"
  cgo = false
  goos = "linux"
  goarch = "arm64"
}

test = go.test {
  package = "./..."
  flags = ["-race"]
}

lint = go.lint {}

# Docker auto-detects COPY/ADD sources and expands ARG/ENV vars
image = docker.image {
  tag = "myapp:latest"
  dockerfile = "docker/Dockerfile"
  depends_on = [server-linux]
}

# bit tracks container state like Terraform — detects drift, replaces on config change
app = docker.container {
  image = image.ref
  name = "myapp"
  ports = ["8080:8080"]
  healthcheck = "curl -sf http://localhost:8080/health"
}

target build = [server]
target test = [test, lint]
target deploy = [app]
```

```sh
bit              # runs `apply` (default)
bit plan         # show what would change
bit apply build  # apply a specific target
bit test         # run test blocks
bit destroy      # remove outputs
bit list         # list targets
bit dump         # show evaluated inputs/outputs
```

## Language

### Variables and Parameters

```bit
let version = "1.0.0"
let git_sha = exec("git rev-parse --short HEAD") | trim

param environment : string
param replicas : int = 1
```

### Blocks

```bit
name = provider.resource {
  field = "value"
  other = [1, 2, 3]
}
```

Special fields:
- `depends_on = [block, ...]` — content-coupled dependency (changes propagate)
- `after = [block, ...]` — ordering-only dependency

Prefix with `protected` to prevent replacement/destruction:

```bit
protected db = docker.container { ... }
```

### Strings

Double-quoted with `${expr}` interpolation and heredocs:

```bit
greeting = "hello ${name}"
script = <<-EOF
  echo ${app.path}
  echo "done"
EOF
```

### Expressions

```bit
list1 + list2           # list concatenation
a == b                  # equality / inequality
expr | trim             # pipes
if cond then a else b   # conditionals
func(arg1, arg2)        # function calls
block.field             # block output references
```

### Built-in Functions

| Function | Description |
|---|---|
| `env(name)`, `env(name, default)` | Environment variable |
| `exec(command)` | Run shell command, return stdout |
| `glob(pattern)` | Expand filesystem glob |
| `trim(value)` | Strip whitespace |
| `lines(string)` | Split into lines |
| `split(string, sep)` | Split by separator |
| `uniq(list)` | Deduplicate list |
| `secret(name)` | Access secret |

### Targets and Outputs

```bit
target build = [app, lib]
output version = app.version
```

## Providers

### exec

General-purpose shell commands.

**`exec`** (build) — run a command, track inputs/outputs:
```bit
app = exec {
  command = "make build"
  output = "bin/app"       # single string or list
  inputs = ["src/**/*.c"]  # glob patterns
}
```

**`exec.test`** (test) — run a command, pass/fail by exit code:
```bit
test = exec.test {
  command = "make test"
  inputs = ["src/**/*.c"]
  format = "ctrf"          # optional: parse CTRF JSON from stdout
  transform = ".results"   # optional: jq expression for CTRF
}
```

### go

Go-aware provider with automatic input detection via source scanning (parses imports, `//go:embed`, `go.mod`/`go.sum`). Results are cached across blocks.

**`go.exe`** — build a Go binary:
```bit
app = go.exe {
  package = "./cmd/app"
  output = "dist/app"      # optional, defaults to package base name
  flags = ["-ldflags=-s"]  # optional
  goos = "linux"           # optional
  goarch = "arm64"         # optional
  cgo = false              # optional
}
```

**`go.build`** — compile without producing a binary:
```bit
check = go.build {
  package = "./..."
}
```

**`go.test`** — run tests:
```bit
test = go.test {
  package = "./..."
  flags = ["-timeout", "30s", "-race"]
}
```

**`go.lint`** — run `golangci-lint`:
```bit
lint = go.lint {}                  # defaults to ./...
lint = go.lint { package = "./cmd/app" }
```

### docker

**`docker.image`** — build a Docker image (auto-detects inputs from Dockerfile COPY/ADD):
```bit
image = docker.image {
  tag = "myapp:latest"
  context = "."
  dockerfile = "Dockerfile"
  build_args = { VERSION = "1.0" }
}
```

**`docker.container`** — run a Docker container:
```bit
app = docker.container {
  image = image.ref
  name = "myapp"
  ports = ["8080:8080"]
  volumes = ["/data:/data"]
  environment = { DB_HOST = "localhost" }
  healthcheck = "curl -sf http://localhost:8080/health"
}
```

## How It Works

1. Parse `BUILD.bit` and build a dependency DAG
2. For each block in topological order:
   - Evaluate field expressions (with upstream outputs in scope)
   - Resolve input files via the provider
   - Compute a content hash (file contents + dependency hashes)
   - Skip if nothing changed; apply if inputs differ
3. Persist state to `.bit/state/state.json`

Parallel execution with `-j N` (defaults to CPU count).
