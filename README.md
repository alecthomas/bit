# bit — Build It

A declarative build tool with dependency tracking, content-based caching, and parallel execution.

bit reads a `BUILD.bit` file, resolves dependencies between blocks, detects what changed, and only rebuilds what's needed. Language-aware providers (e.g. Go, Rust, Docker) automatically discover inputs from source files, so in most cases you don't need to specify them manually.

Like Terraform, bit tracks the state of each block between runs. It detects drift (e.g. a deleted Docker image or stopped container), determines what actions are needed (create, update, destroy), and applies only the minimum changes. `bit --plan` shows what would change; `bit` makes it so; `bit --clean` tears it down.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/alecthomas/bit/refs/heads/master/install.sh | sh
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

# bit tracks container state like Terraform — detects drift, rebuilds on config change
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
bit              # apply the default target (or all blocks if no default)
bit ...          # apply every block regardless of the default target
bit build        # apply a specific target or block
bit --plan       # show what would change
bit --test       # run test blocks
bit --clean      # destroy in reverse dependency order
bit --list       # list all blocks
bit --dump       # show evaluated inputs/stored outputs
bit --info       # show parameters, targets, and outputs
bit --schema     # show provider/resource schemas
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

Prefix with `protected` to prevent destruction:

```bit
protected db = docker.container { ... }
```

If a `default` target is defined, `bit` with no arguments runs only that target. Pass explicit targets or block names (`bit build release`) to run a specific subset, or `bit ...` to run every block regardless of the default.

```bit
target default = [server, test]
```

### Phases

Blocks can be assigned to a phase with `pre` or `post` modifiers. All `pre` blocks complete before any default block starts; all default blocks complete before any `post` block starts. Within each phase, normal dependency ordering applies.

```bit
pre fmt = rust.fmt {}       # runs before everything
post report = exec { ... }  # runs after everything

debug = rust.exe {}         # default phase, waits for fmt
test = rust.test {}         # default phase, waits for fmt
```

### Matrix Expansion

Expand a block over list values with `name[key]`:

```bit
let arch = ["amd64", "arm64"]

binary[arch] = go.exe {
  package = "./cmd/server"
  goarch  = arch             # scalar within each expansion
}

container[arch] = docker.container {
  image = image.ref          # resolves to matching arch slice
  name  = "app-${arch}"
}
```

Creates `binary[amd64]`, `binary[arm64]`, etc. Multiple keys produce a cartesian product. Non-matrix blocks depending on a matrix block wait for all slices.

### Strings

Double-quoted with `${expr}` interpolation, single-quoted raw strings, and heredocs:

```bit
greeting = "hello ${name}"
pattern = 'no \escapes or ${interpolation}'
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
| `basename(path)` | Extract file name from path |
| `dirname(path)` | Extract directory from path |
| `prefix(value, str)` | Prepend string to value or list elements |
| `suffix(value, str)` | Append string to value or list elements |
| `secret(name)` | Access secret |

### Modules

A `.bit` file in `.bit/modules/` becomes a reusable provider. Each directory is a provider namespace, each file a resource. For example `.bit/modules/app/app.bit` maps to `app`:

```bit
# .bit/modules/app/app.bit
param environment : string
param replicas    : int = 1

server = go.exe { package = "./cmd/server" }

image = docker.image {
  tag = "myapp:${environment}"
  depends_on = [server]
}

deploy = docker.container {
  image    = image.ref
  replicas = replicas
}

output endpoint = deploy.endpoint

target deploy = [deploy]
```

Use it like any other provider:

```bit
staging = app {
  environment = "staging"
  replicas    = 2
}

production = app {
  environment = "production"
  replicas    = 10
  depends_on  = [staging]
}

# Access outputs: staging.endpoint, production.endpoint
# Inner blocks are private: staging.server, staging.image are not accessible
```

Two instances of the same module produce independent subgraphs. Modules nest arbitrarily.

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

**`go.fmt`** — format Go source files with `gofmt`:
```bit
pre fmt = go.fmt {}                # defaults to ./...
```

**`go.fmt-l`** — check Go source formatting (test, fails if unformatted):
```bit
fmt-check = go.fmt-l {}
```

### rust

Rust-aware provider. Uses `cargo metadata` to discover local package source directories for input tracking. Shared inputs: `package`, `flags`, `features`, `all_features`, `target`, `profile`, `toolchain`.

**`rust.exe`** — build a Rust binary:
```bit
app = rust.exe {
  bin = "myapp"            # optional, inferred from Cargo.toml
  package = "my-crate"     # optional, for workspaces
  profile = "release"      # optional
}
```
Outputs `path` — the binary location, discovered from cargo's JSON output.

**`rust.build`** — compile without producing a binary:
```bit
check = rust.build {}
```

**`rust.test`** — run tests:
```bit
test = rust.test {
  verbose = true           # optional, show individual test results
}
```

**`rust.clippy`** — run Clippy linter:
```bit
clippy = rust.clippy {}
```

**`rust.fmt`** — format Rust source files with `cargo fmt`:
```bit
pre fmt = rust.fmt {}
```

**`rust.fmt-check`** — check Rust formatting (test, fails if unformatted):
```bit
fmt-check = rust.fmt-check {}
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
   - Module blocks (from `.bit/modules/`) are expanded into namespaced inner blocks
   - Phase modifiers (`pre`/`post`) add synthetic ordering edges between phases
2. For each block in topological order:
   - Evaluate field expressions (with upstream outputs in scope)
   - Resolve input files via the provider
   - Compute a content hash (file contents + dependency hashes)
   - Skip if nothing changed; apply if inputs differ
3. Persist state to the user's cache directory (e.g. `~/Library/Caches/bit/<hash>/state.json` on macOS, `~/.cache/bit/<hash>/state.json` on Linux), partitioned by a hash of the project's absolute path

Parallel execution with `-j N` (defaults to CPU count).
