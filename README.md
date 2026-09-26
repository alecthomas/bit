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

unit-test = go.test {
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
target test = [unit-test, lint]
target deploy = [app]
```

```sh
bit              # apply the default target (or every non-`explicit` block if no default)
bit ...          # apply every non-`explicit` block regardless of the default target
bit build        # apply a specific target or block
bit publish tag=v1  # pass a named argument to a target
bit --force      # rebuild selected blocks without change detection or cache
bit --plan       # show what would change
bit --test       # run test blocks
bit --clean      # destroy targets and their dependents in reverse topological order
bit --list       # list explicitly defined targets
bit -ll          # list all blocks
bit --graph      # render the DAG as an ASCII graph
bit --plan --graph  # …and colour each node by its planned action
bit --dump       # show evaluated inputs/stored outputs
bit --info       # show parameters, targets, and outputs
bit --help       # show CLI options and a BUILD.bit language overview
bit --schema [name]  # show built-in functions and provider resource/function schemas
bit --update [repo...]  # re-resolve imports and rewrite BUILD.bit.lock
bit --fmt        # canonically format the project's BUILD.bit, preserving comments
bit --fmt path/to/file.bit  # format a specified .bit file in place
bit --cache      # show the size of the shared build cache
bit --cache --clean  # delete every cached receipt and artifact
bit --long       # disable live scrolling regions and stream every output line
bit --quiet      # suppress output unless an error occurs (also -q)
bit --since origin/master  # apply blocks affected since the branch point
```

`--fmt` separates top-level declarations with one empty line and puts each
block field on its own line. It retains comments and their attachment to
declarations, as well as expression spelling such as heredocs and quoted strings.

All block/target-taking modes (`bit`, `--plan`, `--clean`, `--graph`, `--dump`)
accept the same positional selector: no argument uses the `default` target
(or every non-`explicit` block if none), `...` forces every non-`explicit`
block, or name one or more targets/blocks to scope the operation. `--clean
<name>` destroys that block plus anything that depends on it, in reverse
topological order.

`--force` (`-f`) rebuilds every block selected by a normal apply, ignoring
change detection and the shared cache. With `--clean`, it instead allows
protected blocks to be destroyed and continues cleanup past errors.

`--since <ref>` limits apply, plan, test, graph, dump, and block-list operations to
blocks affected by changes between `merge-base(<ref>, HEAD)` and the current
worktree. Committed, staged, unstaged, and untracked changes are included. A
block is affected when a changed path is one of its provider-resolved inputs,
or when it content-depends on an affected block. Required prerequisites are
included. `bit -ll --since <ref>` prints affected blocks across the full DAG;
positional targets optionally narrow that list. `-l --since` is rejected
because `-l` lists target definitions rather than blocks.

Blocks without source paths are always included because Git cannot prove them
unaffected. A changed `.bit` build definition selects the full requested target
set. An unowned deleted path also selects the full set when prior state cannot
identify its block. `--since` requires the ref and its merge base to exist
locally, and cannot be combined with `--clean`.

## Language

### Variables and Module Parameters

```hcl
let version = "1.0.0"
let git_sha = exec("git rev-parse --short HEAD") | trim

param environment : string
param replicas : int = 1
```

Top-level `param` declarations are inputs to the `.bit` module. Supply them
with `-P`, for example `bit -P environment=production deploy`.

### Blocks

```hcl
name = provider.resource {
  field = "value"
  other = [1, 2, 3]
}
```

Blocks can declare typed parameters. A parameter may have a default, in which
case its type can be inferred:

```hcl
package(name : string, profile = "release") = exec {
  command = "cargo build --package #{name} --profile #{profile}"
}
```

Parameterized blocks are instantiated by a target call or by naming the block
on the command line. The bound values form part of the instance identity, so
equal calls share one graph node while different values have independent state.

A parameterized block can also be referenced anywhere an ordinary block can.
Pass named arguments before selecting an output, or use the call directly in
`depends_on` and `after`:

```hcl
release(name : string) = exec {
  command = package(name = name).path
  depends_on = [test(name = name)]
  after = [prepare(name = name)]
}
```

Arguments are configuration-time expressions in the caller's scope. Repeating
a call with equal bound values refers to the same graph node.

Special fields:

- `depends_on = [block, ...]` — content-coupled dependency (changes propagate)
- `after = [block, ...]` — ordering-only dependency
- `concurrency = N` — maximum number of this block's matrix slices that may run at once
- `uncached = [output, ...]` — outputs to keep out of the [shared build cache](#shared-build-cache)

Prefix with `protected` to prevent destruction, `explicit` to exclude from `...`, or both (in either order):

```hcl
protected db = docker.container { ... }
explicit migrate = exec { ... }
protected explicit prod_db = aws.aurora { ... }
```

`explicit` blocks are skipped by the `...` selector (and the no-target/no-`default` fallback) for every action, including `--clean ...`. They still run when named directly or pulled in as a dependency of another selected block.

If a `default` target is defined, `bit` with no arguments runs only that target. Pass explicit targets or block names (`bit build release`) to run a specific subset, or `bit ...` to run every non-`explicit` block regardless of the default.
Params, variables, blocks, and targets must have distinct names within a `.bit` file.

```hcl
target default = [server, test]
```

Targets can also declare parameters and pass them explicitly to blocks or
other targets:

```hcl
target publish(name : string, profile = "release") = [
  package(name = name, profile = profile),
]
```

Invoke a parameterized target with named `name=value` arguments immediately
after its name:

```sh
bit publish name=server
bit --plan publish name=server profile=debug
bit publish name=server test package=api
```

An argument belongs to the preceding target or block. A bare word begins the
next selection, so existing multi-target invocations such as `bit build test`
retain their meaning. Required arguments must be supplied. Defaults may refer
to parameters declared earlier in the same parameter list.

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
  concurrency = 1
  package = "./cmd/server"
  goarch  = arch             # scalar within each expansion
}

container[arch] = docker.container {
  image = image.ref          # resolves to matching arch slice
  name  = "app-#{arch}"
}
```

Creates `binary["amd64"]`, `binary["arm64"]`, etc. Matrix keys retain their
`.bit` types, so strings are quoted while numbers, booleans, and block
references keep their respective literal forms.
Multiple keys produce a cartesian product. Non-matrix blocks depending on a
matrix block wait for all slices. Targets can name a matrix block to include
all of its slices.

Matrix slice selectors are expressions. For example, `binary[selected_arch].path`
evaluates `selected_arch` and selects the slice with that typed key.

Set `concurrency` to a positive integer to cap simultaneous slices from that
matrix block without reducing parallelism for unrelated blocks. The global
`-j` limit still caps the total number of running blocks.

### Strings

Double-quoted with `#{expr}` interpolation, single-quoted raw strings, and heredocs:

```hcl
greeting = "hello #{name}"
pattern = 'no \escapes or #{interpolation}'
script = <<-EOF
  echo #{app.path}
  echo "done"
EOF
```

### Expressions

Lists and maps can span lines and may end with a trailing comma. Map values
may have different types. A key can declare its value type; `int` is an alias
for `number`. An annotated key can have a `null` value:

```bit
let items = [
  "bar",
]
let settings = {
  name: string = "bar",
  retries = 2,
  age: int = null,
}
```

```hcl
list1 + list2           # list concatenation
a == b                  # equality / inequality
expr | trim             # pipes
if cond then a else b   # conditionals
func(arg1, arg2)        # function calls
block.field             # block output references
block(name = value)     # parameterized block references
block(name = value).out # parameterized block output references
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

`bit --schema` lists these functions and their signatures. Filter to one with
its unqualified name, for example `bit --schema env`.

**`basename(path: string | [string]) -> string | [string]`** — Extract file names from a path or list of paths.

**`dirname(path: string | [string]) -> string | [string]`** — Extract directories from a path or list of paths.

**`env(name: string, default: any?) -> string | any`** — Read an environment variable, with an optional fallback.

**`exec(command: string) -> string`** — Run a shell command and return stdout.

**`glob(pattern: string) -> [string]`** — Expand a filesystem glob.

**`lines(value: string) -> [string]`** — Split a string into nonempty lines.

**`prefix(value: string | [string], text: string) -> string | [string]`** — Prepend text to a string or each string in a list.

**`secret(name: string) -> string`** — Read a secret by name.

**`sha256(value: string) -> string`** — Hash a string with SHA-256.

**`split(value: string, separator: string) -> [string]`** — Split a string by a separator.

**`suffix(value: string | [string], text: string) -> string | [string]`** — Append text to a string or each string in a list.

**`trim(value: string | [string]) -> string | [string]`** — Trim whitespace from a string or each string in a list.

**`uniq(list: [any]) -> [any]`** — Deduplicate a list while preserving order.

### Provider Functions

Providers can expose functions for configuration-time discovery. Their results
can feed matrix expansion so each discovered package becomes an independent
block:

```bit
let package = go.packages("./...")

tests[package] = go.test {
  package = package
}
```

Provider functions can also return typed block references for dependency
fields. `rust.dependencies` returns immediate local non-development
dependencies; each `$` in its optional template is replaced with the
dependency package name:

```bit
let package = rust.packages()

crate[package] = rust.build {
  package = package
  depends_on = rust.dependencies(package, "crate[$]")
}
```

`bit --schema` includes these function signatures and their descriptions.
Filter by provider or exact member, such as `bit --schema go` or
`bit --schema go.packages`. The generated provider reference below includes
the same function metadata.

### Modules

Each `import` brings exactly one provider into scope; the provider name is the last segment of the import path. The imported directory contains one `<resource>.bit` file per resource. For example, `./.bit/modules/app/` imported as `app` provider, with `app/app.bit` as the default resource and `app/staging.bit` exposed as `app.staging`:

```hcl
# .bit/modules/app/app.bit
param environment : string
param replicas    : int = 1

server = go.exe { package = "./cmd/server" }

image = docker.image {
  tag = "myapp:#{environment}"
  depends_on = [server]
}

service = docker.container {
  image    = image.ref
  replicas = replicas
}

output endpoint = service.endpoint

target deploy = [service]
```

Use it like any other provider:

```hcl
import "./.bit/modules/app"

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

A `<provider>.bit` file at the root of the import directory is the **default** resource — callable bare (just `app { ... }`). All other `<name>.bit` files are addressable as `<provider>.<name>` (e.g. `app.staging`). Two instances of the same module produce independent subgraphs. Modules nest arbitrarily.

#### Imports

Each top-level `import` directive brings exactly one provider into scope. The provider name is the last segment of the import path. Local directories use `./` or `../` relative paths; git imports are bare `host/path` (à la Go module paths) — no schemes (`https://`, `ssh://`, `file://`), no SSH shorthand (`git@host:`), no `#ref`, no host allowlist, no absolute paths:

```hcl
import "./.bit/modules/docker"                       # provider "docker"
import "../shared/aws"                               # provider "aws"
import "github.com/alecthomas/bit-modules"           # provider "bit-modules"
import "github.com/alecthomas/bit-modules/aws"       # provider "aws" — subpath inside the repo
import "git.sr.ht/~user/repo"                        # arbitrary host
import "github.com/alecthomas/bit-modules" as bm     # override the auto-derived provider name
```

For deep paths like `github.com/foo/bar/waz`, bit needs to know where the repo ends and the subpath begins. Hardcoded forges (`github.com`, `gitlab.com`, `bitbucket.org`) split at segment 3 (`host/owner/repo`). For other hosts bit probes via `git ls-remote` from the longest prefix down — the first one that responds is the repo, the rest is the subpath. The probe result is cached for the run but not persisted, so a fresh checkout against an unfamiliar host pays one probe per import.

Use `as <ident>` to override the auto-derived provider name — useful when two imports would otherwise collide on the last path segment.

Git URLs are always cloned as `https://<url>`; for SSH or custom auth, add `insteadOf` rules to your `~/.gitconfig` (bit shells out to `git clone`, which respects them). A trailing `.git` is optional and stripped from the lock key.

##### Recursive resolution and `BUILD.bit.lock`

Every project — the root and every imported project — has its own `BUILD.bit` listing **its** direct imports, and its own `BUILD.bit.lock` pinning those git deps. Resolution walks the graph: root's imports first, then each imported project's `BUILD.bit` imports, transitively.

- The **root** project's lock is writable: new git imports are auto-pinned to the current default-branch HEAD, and `bit --update` re-resolves entries.
- **Child** projects' locks are read-only. Every git import a child declares **must** have a matching entry in that child's `BUILD.bit.lock`, or bit errors. A child with no `BUILD.bit` is a leaf — no further deps to resolve.
- Imports of imports become providers in the root project's scope, so any module can use any transitively imported provider.
- Conflicts are hard errors:
  - same git repo resolved to two different SHAs anywhere in the graph;
  - two unrelated imports producing the same provider name.

Lock file format (commit to version control):

```toml
"github.com/alecthomas/bit-modules" = "a1b2c3d4e5f6..."
```

A normal `bit` run honours the root lock and only contacts the network when a new direct import has no entry yet. `bit --update [repo...]` re-resolves matching entries against the default branch and rewrites the root lock; child locks aren't touched. Each resolved commit is extracted into an immutable per-commit cache directory (`~/Library/Caches/bit/` on macOS, `~/.cache/bit/` on Linux), so re-using a pinned commit is fully offline.

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

| Field      | Type     | Description                                                          |
| ---------- | -------- | -------------------------------------------------------------------- |
| `ref`      | `string` | Locally pinned tag or registry digest reference                      |
| `image_id` | `string` | Docker image ID or multi-platform manifest digest, without `sha256:` |

**`docker.push`** (build) — Push a Docker image to a registry

```bit
block = docker.push {
  image = string  # Source image reference (e.g. from a docker.image block's ref)
  tag = string    # Destination tag including registry (e.g. "localhost:5000/app:abc123")
}
```

**Outputs:**

| Field | Type     | Description            |
| ----- | -------- | ---------------------- |
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

| Field          | Type     | Description         |
| -------------- | -------- | ------------------- |
| `container_id` | `string` | Docker container ID |
| `name`         | `string` | Container name      |

**`docker.network`** (build) — Create a Docker network (Terraform-style: tracked state, drift detection)

```bit
block = docker.network {
  name = string     # Network name (must be unique per daemon)
  driver = string?  # Network driver (bridge, host, overlay, ...). Defaults to `bridge`.
}
```

**Outputs:**

| Field  | Type     | Description       |
| ------ | -------- | ----------------- |
| `name` | `string` | Network name      |
| `id`   | `string` | Docker network ID |

**`docker.network_attach`** (build) — Attach a container to a Docker network (equivalent of `docker network connect`). Mirrors every flag of the underlying CLI so this block fully replaces a hand-rolled `exec` wrapper. The attachment is idempotent and is tracked in state so drift detection works across runs.

```bit
block = docker.network_attach {
  network = string                  # Network name or ID
  container = string                # Container name or ID
  aliases = [string]?               # Network-scoped aliases for the container (`--alias`)
  driver_opts = {string = string}?  # Driver options as key/value pairs (`--driver-opt`)
  gw_priority = number?             # Default-gateway priority on this endpoint (`--gw-priority`). Highest priority provides the default gateway; accepts negative values.
  ip = string?                      # IPv4 address (`--ip`)
  ip6 = string?                     # IPv6 address (`--ip6`)
  links = [string]?                 # Links to other containers, in `name:alias` form (`--link`)
  link_local_ips = [string]?        # Link-local addresses for the container (`--link-local-ip`)
}
```

**Outputs:**

| Field         | Type      | Description                                           |
| ------------- | --------- | ----------------------------------------------------- |
| `ip_address`  | `string?` | IPv4 address Docker assigned to the endpoint, if any. |
| `ip6_address` | `string?` | IPv6 address Docker assigned to the endpoint, if any. |

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

| Field   | Type        | Description                        |
| ------- | ----------- | ---------------------------------- |
| `path`  | `string?`   | Output path (single-output blocks) |
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

| Field    | Type   | Description             |
| -------- | ------ | ----------------------- |
| `passed` | `bool` | Whether the test passed |

### go

**`go.exe`** (build) — Build a Go binary

```bit
block = go.exe {
  package = string   # Go package to build (e.g. "./cmd/myapp")
  output = string?   # Output binary path (defaults to package base name)
  flags = [string]?  # Extra flags passed to go build
  dir = string?      # Working directory for the command
  goos = string?     # Target OS
  goarch = string?   # Target architecture
  cgo = bool?        # Enable cgo
}
```

**Outputs:**

| Field  | Type     | Description              |
| ------ | -------- | ------------------------ |
| `path` | `string` | Path to the built binary |

**`go.build`** (build) — Compile Go packages without producing a binary

```bit
block = go.build {
  package = string   # Go package pattern (e.g. "./...")
  flags = [string]?  # Extra flags passed to go build
  dir = string?      # Working directory for the command
  goos = string?     # Target OS
  goarch = string?   # Target architecture
  cgo = bool?        # Enable cgo
}
```

**`go.generate`** (build) — Run go generate

```bit
block = go.generate {
  package = string     # Go package pattern (e.g. "./...")
  flags = [string]?    # Extra flags passed to go generate
  inputs = [string]?   # Input file glob patterns (in addition to Go sources)
  outputs = [string]?  # Output file paths produced by generate commands
  dir = string?        # Working directory for the command
  goos = string?       # Target OS
  goarch = string?     # Target architecture
  cgo = bool?          # Enable cgo
}
```

**`go.test`** (test) — Run Go tests

```bit
block = go.test {
  package = string   # Go package pattern (e.g. "./...")
  flags = [string]?  # Extra flags passed to go test
  verbose = bool?    # Show individual test results
  dir = string?      # Working directory for the command
  goos = string?     # Target OS
  goarch = string?   # Target architecture
  cgo = bool?        # Enable cgo
}
```

**Outputs:**

| Field    | Type   | Description              |
| -------- | ------ | ------------------------ |
| `passed` | `bool` | Whether all tests passed |

**`go.lint`** (test) — Run golangci-lint

```bit
block = go.lint {
  package = string   # Go package pattern
  flags = [string]?  # Extra flags passed to golangci-lint run
  dir = string?      # Working directory for the command
}
```

**Outputs:**

| Field    | Type   | Description            |
| -------- | ------ | ---------------------- |
| `passed` | `bool` | Whether linting passed |

**`go.fmt`** (build) — Format Go source files

```bit
block = go.fmt {
  package = string  # Go package pattern
  dir = string?     # Working directory for the command
}
```

**`go.fmt-l`** (test) — Format Go source files

```bit
block = go.fmt-l {
  package = string  # Go package pattern
  dir = string?     # Working directory for the command
}
```

**Outputs:**

| Field    | Type   | Description                     |
| -------- | ------ | ------------------------------- |
| `passed` | `bool` | Whether all files are formatted |

**`go.packages(pattern: string, dir: string?) -> [string]`** — List Go packages matching a package pattern.

### pnpm

**`pnpm.install`** (build) — Install pnpm workspace dependencies.

```bit
block = pnpm.install {
  dir = string    # Workspace root directory (defaults to the current directory)
  frozen = bool?  # Pass `--frozen-lockfile` (reproducible installs, default `true`)
}
```

**Outputs:**

| Field  | Type     | Description                                             |
| ------ | -------- | ------------------------------------------------------- |
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

| Field   | Type        | Description                                            |
| ------- | ----------- | ------------------------------------------------------ |
| `path`  | `string?`   | Single output path, when exactly one was declared      |
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

| Field    | Type   | Description                          |
| -------- | ------ | ------------------------------------ |
| `passed` | `bool` | Whether the test command exited zero |

**`pnpm.packages_with_script(script: string, dir: string?) -> [string]`** — List workspace packages that define a script.

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

| Field  | Type     | Description              |
| ------ | -------- | ------------------------ |
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

| Field    | Type   | Description              |
| -------- | ------ | ------------------------ |
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

| Field    | Type   | Description              |
| -------- | ------ | ------------------------ |
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

| Field    | Type   | Description              |
| -------- | ------ | ------------------------ |
| `passed` | `bool` | Whether the check passed |

**`rust.packages() -> [string]`** — List Cargo workspace packages.

**`rust.dependencies(package: string, template: string?) -> [block]`** — List a Cargo workspace package's immediate local non-development dependencies. `package` names the Cargo workspace package. When `template` is set, every `$` in it is replaced with the dependency package name. For example, `crate[$]` returns matrix block references. Development dependencies are excluded because Cargo does not build them for `cargo build` and permits cycles through them.

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

Rust resources with a `package` select that package and its transitive local
workspace dependencies for change detection. Without `package`, they track the
whole workspace. All Rust resources also track `rustfmt.toml` and
`.cargo/config.toml` at the workspace root.

### Shared build cache

Successful results of some resources are also recorded in a shared cache so
that linked Git worktrees of the same repository can reuse them, even after
the worktree that produced them has been deleted:

- `rust.build`, `rust.test`, `rust.clippy`, `rust.fmt`, `rust.fmt-check`,
  `go.build`, `go.test`, `go.lint`, `go.fmt`, and `go.fmt-l` record that the
  action succeeded for a given set of sources, inputs, dependencies, and
  toolchain. A worktree with identical sources skips the action.
- `go.exe` and `rust.exe` additionally store the built binary. A worktree
  with identical sources restores it to its own output path (for `rust.exe`,
  the same path under its own `target/` directory) instead of building.
  `bit --plan` reports this as a restore (`⇣`) without writing anything.
- Single-platform `docker.image` builds store the members of a Docker image
  archive separately, so image versions that share layers also share their
  cached bytes. If the image has been removed, an identical action restores it
  with `docker image load` instead of rebuilding it. Multi-platform builds are
  pushed directly to their registry and remain in worktree-local state because
  they cannot be restored through the local image store.
- `exec` and `exec.test` store whatever they declare in `output`. A directory
  is stored whole, as the set of files it contained, and is restored to
  exactly that — anything already at the path is replaced, not merged. An
  `exec` block that sets `resolve` or `outputs` is never shared: those fields
  describe state outside the worktree, which a result recorded elsewhere
  cannot speak for.

A directory captured this way must contain only ordinary files and
directories. If it holds a symlink the block still runs and succeeds, but bit
reports that it could not be cached rather than restoring something the
command never produced.

To keep a particular output out of the cache, name it in `uncached`:

```hcl
build = exec {
  command  = "pnpm build"
  output   = ["dist/", "node_modules/"]
  uncached = ["node_modules/"]
}
```

The block still caches `dist/`. Excluding an output does not change when the
block is considered up to date, only what is stored, so a worktree that reuses
this result gets `dist/` and no `node_modules/`. Exclude an output when it is
large and reproducible by other means, not when later blocks need it.

So that Rust binaries built in different worktrees are interchangeable, bit
compiles workspace crates with `--remap-path-prefix` rewriting the worktree
root to the repository's main worktree. Panic locations and debug info
therefore point at the main checkout rather than the worktree that happened
to build them. bit applies this through Cargo's workspace wrapper, so
`.cargo/config.toml` rustflags are untouched and dependencies are compiled
exactly as before. Workspace crates themselves are recompiled when switching
between bit and plain `cargo` commands in the same worktree.

Only linked worktrees of one repository share entries, and only when the
`BUILD.bit` sits at the same path relative to the worktree root. Failed tests
and lint runs are never shared. Restored files are independent copies, so
editing or deleting one cannot affect the cache. `bit --clean` removes only
the worktree's own outputs and state. `bit --cache` shows how much the
shared cache holds and `bit --cache --clean` deletes all of it, for every
project. It lives under `~/Library/Caches/bit` (or `~/.cache/bit` on Linux);
set `BIT_CACHE_DIR` to relocate it.
