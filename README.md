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

```hcl
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
bit --clean      # destroy targets and their dependents in reverse topological order
bit --list       # list all blocks
bit --graph      # render the DAG as an ASCII graph
bit --plan --graph  # …and colour each node by its planned action
bit --dump       # show evaluated inputs/stored outputs
bit --info       # show parameters, targets, and outputs
bit --schema     # show provider/resource schemas
```

All block/target-taking modes (`bit`, `--plan`, `--clean`, `--graph`, `--dump`)
accept the same positional selector: no argument uses the `default` target
(or every block if none), `...` forces every block, or name one or more
targets/blocks to scope the operation. `--clean <name>` destroys that block
plus anything that depends on it, in reverse topological order.

## Language

### Variables and Parameters

```hcl
let version = "1.0.0"
let git_sha = exec("git rev-parse --short HEAD") | trim

param environment : string
param replicas : int = 1
```

### Blocks

```hcl
name = provider.resource {
  field = "value"
  other = [1, 2, 3]
}
```

Special fields:
- `depends_on = [block, ...]` — content-coupled dependency (changes propagate)
- `after = [block, ...]` — ordering-only dependency

Prefix with `protected` to prevent destruction:

```hcl
protected db = docker.container { ... }
```

If a `default` target is defined, `bit` with no arguments runs only that target. Pass explicit targets or block names (`bit build release`) to run a specific subset, or `bit ...` to run every block regardless of the default.

```hcl
target default = [server, test]
```

### Phases

Blocks can be assigned to a phase with `pre` or `post` modifiers. All `pre` blocks complete before any default block starts; all default blocks complete before any `post` block starts. Within each phase, normal dependency ordering applies.

```hcl
pre fmt = rust.fmt {}       # runs before everything
post report = exec { ... }  # runs after everything

debug = rust.exe {}         # default phase, waits for fmt
test = rust.test {}         # default phase, waits for fmt
```

### Matrix Expansion

Expand a block over list values with `name[key]`:

```hcl
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

```hcl
greeting = "hello ${name}"
pattern = 'no \escapes or ${interpolation}'
script = <<-EOF
  echo ${app.path}
  echo "done"
EOF
```

### Expressions

```hcl
list1 + list2           # list concatenation
a == b                  # equality / inequality
expr | trim             # pipes
if cond then a else b   # conditionals
func(arg1, arg2)        # function calls
block.field             # block output references
```

### Durations

Duration literals are unquoted and fuse a number with a unit suffix — no whitespace between them:

```hcl
interval = 5s
timeout  = 500ms
budget   = 1.5h
```

Supported units: `ns`, `us`, `ms`, `s`, `m`, `h`, `d`. A bare `5` is a number; `5s` is a duration.

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
| `sha256(value)` | Hex-encoded SHA-256 digest |

### Modules

A `.bit` file in `.bit/modules/` becomes a reusable provider. Each directory is a provider namespace, each file a resource. For example `.bit/modules/app/app.bit` maps to `app`:

```hcl
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

```hcl
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

```hcl
target build = [app, lib]
output version = app.version
```

## Providers

### docker

**`docker.image`** (build) — Build a Docker image (auto-detects inputs from Dockerfile)

```bit
block = docker.image {
  tag = string                     # Image tag
  context = string                 # Build context directory
  dockerfile = string              # Dockerfile path
  build_args = {string = string}?  # Docker build arguments
  platform = [string]?             # Target platform(s)
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `ref` | `string` | Image tag/reference |
| `image_id` | `string` | Docker image ID |

**`docker.push`** (build) — Push a Docker image to a registry

```bit
block = docker.push {
  image = string  # Source image reference (e.g. from a docker.image block's ref)
  tag = string    # Destination tag including registry (e.g. "localhost:5000/app:abc123")
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `ref` | `string` | Pushed image reference |

**`docker.container`** (build) — Run a Docker container (tracks state like Terraform)

```bit
block = docker.container {
  image = string                        # Docker image reference
  name = string                         # Container name
  ports = [string]?                     # Port mappings (e.g. "8080:80")
  volumes = [string]?                   # Volume mounts
  environment = {string = string}?      # Environment variables
  command = string?                     # Override CMD
  entrypoint = string?                  # Override ENTRYPOINT
  restart = string                      # Restart policy
  network = string?                     # Docker network
  working_dir = string?                 # Working directory
  healthcheck = string | {test = string, interval = duration, timeout = duration, retries = number, start_period = duration?}?  # Health check command or config
  extra_hosts = {string = string}?      # Extra /etc/hosts entries (hostname → address). On Linux, `host.docker.internal: host-gateway` is auto-added if not present.
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `container_id` | `string` | Docker container ID |
| `name` | `string` | Container name |

**`docker.network`** (build) — Create a Docker network (Terraform-style: tracked state, drift detection)

```bit
block = docker.network {
  name = string     # Network name (must be unique per daemon)
  driver = string?  # Network driver (bridge, host, overlay, ...). Defaults to `bridge`.
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `name` | `string` | Network name |
| `id` | `string` | Docker network ID |

### exec

**`exec`** (build) — Run a shell command, track inputs and outputs

```bit
block = exec {
  command = string    # Shell command to execute
  output = [string]?  # Output file or list of output files
  inputs = [string]?  # Input file glob patterns
  dir = string?       # Working directory for the command
  clean = string?     # Shell command to run on `bit --clean` (replaces the default removal of outputs)
  resolve = string?   # Shell command whose stdout is captured as state. Used to detect whether the resource exists and whether it has drifted.
  outputs = string?   # Shell command whose stdout is parsed as JSON and exposed as block outputs.
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `path` | `string?` | Output path (single-output blocks) |
| `paths` | `[string]?` | Output paths (multi-output blocks) |

**`exec.test`** (test) — Run a command as a test (pass/fail by exit code)

```bit
block = exec.test {
  command = string    # Shell command to execute
  inputs = [string]?  # Input file glob patterns
  output = [string]?  # Output files to track
  dir = string?       # Working directory for the command
  clean = string?     # Shell command to run on `bit --clean`
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether the test passed |

### go

**`go.exe`** (build) — Build a Go binary

```bit
block = go.exe {
  package = string   # Go package to build (e.g. "./cmd/myapp")
  output = string?   # Output binary path (defaults to package base name)
  flags = [string]?  # Extra flags passed to go build
  goos = string?     # Target OS
  goarch = string?   # Target architecture
  cgo = bool?        # Enable cgo
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `path` | `string` | Path to the built binary |

**`go.build`** (build) — Compile Go packages without producing a binary

```bit
block = go.build {
  package = string   # Go package pattern (e.g. "./...")
  flags = [string]?  # Extra flags passed to go build
  goos = string?     # Target OS
  goarch = string?   # Target architecture
  cgo = bool?        # Enable cgo
}
```

**`go.test`** (test) — Run Go tests

```bit
block = go.test {
  package = string   # Go package pattern (e.g. "./...")
  flags = [string]?  # Extra flags passed to go test
  verbose = bool?    # Show individual test results
  goos = string?     # Target OS
  goarch = string?   # Target architecture
  cgo = bool?        # Enable cgo
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether all tests passed |

**`go.lint`** (test) — Run golangci-lint

```bit
block = go.lint {
  package = string   # Go package pattern
  flags = [string]?  # Extra flags passed to golangci-lint run
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether linting passed |

**`go.fmt`** (build) — Format Go source files

```bit
block = go.fmt {
  package = string  # Go package pattern
}
```

**`go.fmt-l`** (test) — Format Go source files

```bit
block = go.fmt-l {
  package = string  # Go package pattern
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether all files are formatted |

### pnpm

**`pnpm.install`** (build) — Install pnpm workspace dependencies.

```bit
block = pnpm.install {
  dir = string    # Workspace root directory (defaults to the current directory)
  frozen = bool?  # Pass `--frozen-lockfile` (reproducible installs, default `true`)
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `path` | `string` | Absolute path to the installed `node_modules` directory |

**`pnpm.run`** (build) — Run a script defined in `package.json`.

```bit
block = pnpm.run {
  script = string     # Script name from `package.json` (e.g. "build")
  package = string?   # Package name from its `package.json`. Omit to run at the workspace root.
  args = [string]?    # Additional arguments passed to the script after `--`
  output = [string]?  # Output file or list of output files/directories produced by the script
  inputs = [string]?  # Extra input file globs (added to auto-detected sources)
  dir = string        # Workspace root directory (defaults to the current directory)
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `path` | `string?` | Single output path, when exactly one was declared |
| `paths` | `[string]?` | Multiple output paths, when more than one was declared |

**`pnpm.test`** (test) — Run a test script via pnpm.

```bit
block = pnpm.test {
  script = string     # Script name from `package.json` (defaults to "test")
  package = string?   # Package name from its `package.json`. Omit to run at the workspace root.
  args = [string]?    # Additional arguments passed to the script after `--`
  inputs = [string]?  # Extra input file globs (added to auto-detected sources)
  dir = string        # Workspace root directory (defaults to the current directory)
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether the test command exited zero |

### rust

**`rust.build`** (build) — Compile Rust packages

```bit
block = rust.build {
  package = string?     # Package to build (-p flag)
  flags = [string]?     # Extra flags passed to cargo build
  features = [string]?  # Features to enable
  all_features = bool?  # Enable all features
  target = string?      # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?     # Build profile (e.g. "release")
  toolchain = string?   # Rust toolchain (e.g. "nightly")
}
```

**`rust.exe`** (build) — Build a Rust binary

```bit
block = rust.exe {
  bin = string?         # Binary target name (inferred if omitted)
  package = string?     # Package containing the binary (-p flag)
  flags = [string]?     # Extra flags passed to cargo build
  features = [string]?  # Features to enable
  all_features = bool?  # Enable all features
  target = string?      # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?     # Build profile (e.g. "release")
  toolchain = string?   # Rust toolchain (e.g. "nightly")
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `path` | `string` | Path to the built binary |

**`rust.test`** (test) — Run Rust tests

```bit
block = rust.test {
  package = string?     # Package to test (-p flag)
  flags = [string]?     # Extra flags passed to cargo test
  verbose = bool?       # Show individual test results
  features = [string]?  # Features to enable
  all_features = bool?  # Enable all features
  target = string?      # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?     # Build profile (e.g. "release")
  toolchain = string?   # Rust toolchain (e.g. "nightly")
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether the check passed |

**`rust.clippy`** (test) — Run Clippy linter

```bit
block = rust.clippy {
  package = string?     # Package to lint (-p flag)
  flags = [string]?     # Extra flags passed to cargo clippy
  features = [string]?  # Features to enable
  all_features = bool?  # Enable all features
  target = string?      # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?     # Build profile (e.g. "release")
  toolchain = string?   # Rust toolchain (e.g. "nightly")
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether the check passed |

**`rust.fmt`** (build) — Format Rust source files

```bit
block = rust.fmt {
  package = string?    # Package to format (-p flag)
  flags = [string]?    # Extra flags passed to cargo fmt
  target = string?     # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?    # Build profile (e.g. "release")
  toolchain = string?  # Rust toolchain (e.g. "nightly")
}
```

**`rust.fmt-check`** (test) — Format Rust source files

```bit
block = rust.fmt-check {
  package = string?    # Package to format (-p flag)
  flags = [string]?    # Extra flags passed to cargo fmt
  target = string?     # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?    # Build profile (e.g. "release")
  toolchain = string?  # Rust toolchain (e.g. "nightly")
}
```

**Outputs:**

| Field | Type | Description |
|---|---|---|
| `passed` | `bool` | Whether the check passed |

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
