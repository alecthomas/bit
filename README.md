# bit — Build It

A declarative build tool with dependency tracking, content-based caching, and parallel execution.

`bit` reads a `BUILD.bit` file, resolves dependencies between blocks, detects what changed, and only rebuilds what's needed. Language-aware providers (e.g. Go, Rust, Docker) automatically discover inputs from source files, so in most cases you don't need to specify them manually.

Like Terraform, `bit` tracks the state of each block between runs. It detects drift (e.g. a deleted Docker image or stopped container), determines what actions are needed (create, update, destroy), and applies only the minimum changes. `bit --plan` shows what would change; `bit` makes it so; `bit --clean` tears it down.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/alecthomas/bit/refs/heads/master/install.sh | sh
```

Or from source:

```sh
cargo install --path .
```

## Quick Start

Put a `BUILD.bit` file at the root of your project. Each block names a
resource from a provider, such as `go.exe`, and sets its fields.

This one builds a Go server, tests and lints it, packages a Linux build into a
Docker image, and runs that image as a container:

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

# Build the server binary
target default = [server]
# Run the tests and linter
target test = [unit-test, lint]
# Build the image and run it as a container
target deploy = [app]
```

`bit -l` lists the targets:

```console
$ bit -l
default — Build the server binary
deploy — Build the image and run it as a container
test — Run the tests and linter
```

`bit` runs the `default` target. A second run skips blocks that haven't
changed:

```console
$ bit
server ▶ go build -o dist/server -trimpath ./cmd/server
server ✔ ok
$ bit
server · no changes
```

### Overview

Blocks depend on each other through `depends_on` or by referencing another
block's outputs, such as `image.ref`. Targets name groups of blocks to run
together, and a comment above a target describes it. The `default` target runs
when `bit` is given no arguments. `bit` runs independent blocks in parallel.

Providers discover a block's inputs and outputs themselves, and many resources
also accept `inputs` and `outputs` for files they can't discover.

`bit` hashes each block's fields, input files, outputs, the tools it runs, and
the blocks it depends on. A block whose hash hasn't changed since it last
succeeded is skipped. A change reruns that block and the blocks that depend on
it.

When a block reruns, its provider sees which files changed and can narrow the
work to them. For example, `go.test` and `go.lint` run only on the packages
that changed files affect, and `exec` passes the changed files to its command
in `BIT_CHANGED_FILES`.

Successful results also go into a [shared build cache](#shared-build-cache).
Another checkout of the project with the same sources reuses them, restoring
build outputs instead of rebuilding. Point `--cache` at an S3 bucket to share
results between machines, such as CI and developers.

### Usage

Run `bit` in the project directory:

```sh
bit              # apply the default target (or every non-`explicit` block if no default)
bit ...          # apply every non-`explicit` block regardless of the default target
bit test         # apply a specific target or block
bit publish tag=v1  # pass a named argument to a target
bit --force      # rebuild selected blocks without change detection or cache
bit --plan       # show what would change
bit --clean      # destroy targets and their dependents in reverse topological order
bit --clean db   # explicitly destroy a protected block
bit --list       # list explicitly defined targets
bit -ll          # list all blocks
bit --graph      # render the DAG as an ASCII graph
bit --plan --graph  # …and colour each node by its planned action
bit --graph --json  # print the DAG as JSON mapping each block to its deps
bit --dump       # show evaluated inputs/stored outputs
bit --info       # show parameters, targets, and outputs
bit --help       # show CLI options and a BUILD.bit language overview
bit --docs [topic]  # print this README, or one section such as `ci`, offline
bit --schema [name]  # show built-in functions and provider resource/function schemas
bit --schema --json  # group schemas by builtins, provider, or imported module
bit --update [repo...]  # re-resolve imports and rewrite BUILD.bit.lock
bit --fmt        # canonically format the project's BUILD.bit, preserving comments
bit --fmt path/to/file.bit  # format a specified .bit file in place
bit --cache s3://bucket/prefix  # also use a remote build cache (or set BIT_CACHE)
bit --cache disk:// --clean  # delete every cached receipt and artifact
bit --long       # disable live scrolling regions and stream every output line
bit --quiet      # suppress output unless an error occurs (also -q)
NO_COLOR=1 bit   # disable colour (output is plain and --long when not a terminal or TERM=dumb)
bit --since origin/master  # apply blocks affected since the branch point
bit --since      # same, using the remote default branch (origin/HEAD)
bit --ci github > .github/workflows/bit.yml  # generate a GitHub Actions workflow
bit --ci ./buildkite.yml.tmpl  # render a CI configuration from a template
```

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
case its type can be inferred. Defaults may also refer to earlier `let` or
`param` declarations, or to earlier parameters in the same block or target:

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
- `dir = path` — directory the block's commands run in, relative to the project root (defaults to the project root). Relative paths in the block's other fields are relative to `dir`. `bit --clean` runs in the `dir` the block last ran in
- `env = { NAME = "value", ... }` — environment variables set for every command the block runs. Changing one reruns the block

Every other field must be an input of the block's resource, as listed under [Providers](#providers) or by `bit --schema`. `bit` checks fields when it loads `BUILD.bit` and rejects unknown fields, missing required fields, and values of the wrong type. A value taken from another block's outputs is checked against that output's declared type, and checked again just before the block runs.

Prefix with `protected` to prevent destruction, `explicit` to exclude from `...`, `large` to mark a long-running or resource-heavy block, or any combination (in any order):

```hcl
protected db = docker.container { ... }
explicit migrate = exec { ... }
protected explicit prod_db = aws.aurora { ... }
large image = docker.image { ... }
```

Blocks without `large` are small. Modifiers on a module instance apply to every block it expands to: a `protected`, `explicit`, or `large` instance makes all of its blocks so, and a `pre` or `post` instance puts all of its blocks in that phase. A block inside a `pre` instance cannot be declared `post`, or vice versa.

`explicit` blocks are skipped by the `...` selector (and the no-target/no-`default` fallback) for every action, including `--clean ...`. They still run when named directly or pulled in as a dependency of another selected block.

If a `default` target is defined, `bit` with no arguments runs only that target. Pass explicit targets or block names (`bit build release`) to run a specific subset, or `bit ...` to run every non-`explicit` block regardless of the default.
Params, variables, blocks, and targets must have distinct names within a `.bit` file.

```hcl
target default = [server, test]
```

Targets can also declare parameters and pass them explicitly to blocks or
other targets:

```hcl
target publish(name : string, profile = "release") = [package(name = name, profile = profile)]
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

A key can compute its own list with `key = expr`. The expression runs once for
each combination of the keys before it, with those keys bound to their slice
values, so a later list can depend on an earlier key:

```hcl
let module = go.modules()

tests[module, package = go.packages("./...", module)] = go.test {
  dir     = module
  package = package
}
```

This creates one slice per package in each module, such as
`tests["svc", "example.com/svc/api"]`. A combination whose list is empty
produces no slices.

Matrix slice selectors are expressions. For example, `binary[selected_arch].path`
evaluates `selected_arch` and selects the slice with that typed key.

On the command line, string keys may be unquoted: `bit 'binary[arm64]'` selects
`binary["arm64"]`. Only identifier keys are read as strings; other keys are
literals, so quote string values such as `"1.2"`.

A matrix block can also declare parameters after its matrix keys. The keys
select slices; named arguments configure an instance of the whole matrix:

```hcl
let arch = ["amd64", "arm64"]

binary[arch](tag : string) = exec {
  command = "build #{arch} with tag #{tag}"
}

target release = [binary(tag = "1.2.3")]
target arm_release = [binary["arm64"](tag = "1.2.3")]
```

`binary(tag = "1.2.3")` selects every slice for that argument set. A slice
call such as `binary["arm64"](tag = "1.2.3")` selects one slice. Both forms can
be used in targets, `depends_on`, and `after`, and a slice call can also be
used in expressions. Equal argument sets share the same slices; different
values create independent instances. Matrix key lists can come from module
bindings or list-valued block parameters.

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

**`filter(list: [string], pattern: string) -> [string]`** — Keep the strings in a list that match a regular expression.

**`glob(pattern: string) -> [string]`** — Expand a filesystem glob.

**`lines(value: string) -> [string]`** — Split a string into nonempty lines.

**`prefix(value: string | [string], text: string) -> string | [string]`** — Prepend text to a string or each string in a list.

**`project_dir() -> string`** — Return the absolute directory containing BUILD.bit.

**`replace(value: string | [string], pattern: string, replacement: string) -> string | [string]`** — Replace every regular expression match in a string or each string in a list. The replacement may reference capture groups as `$1` or `${name}`.

**`secret(name: string) -> string`** — Read a secret by name. `bit` shows the secret as this call, never its value. A secret cannot be passed as a block or target argument.

**`sha256(value: string) -> string`** — Hash a string with SHA-256.

**`shell_join(value: string | [string]) -> string`** — Quote a string, or each string in a list, as POSIX shell words joined by spaces.

**`shell_split(value: string) -> [string]`** — Split a POSIX shell string into its unquoted words.

**`split(value: string, separator: string) -> [string]`** — Split a string by a separator.

**`suffix(value: string | [string], text: string) -> string | [string]`** — Append text to a string or each string in a list.

**`trim(value: string | [string]) -> string | [string]`** — Trim whitespace from a string or each string in a list.

**`trim_prefix(value: string | [string], text: string) -> string | [string]`** — Remove a leading prefix from a string or each string in a list, if present.

**`trim_suffix(value: string | [string], text: string) -> string | [string]`** — Remove a trailing suffix from a string or each string in a list, if present.

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
fields, such as one matrix slice per local dependency:

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

For deep paths like `github.com/foo/bar/waz`, `bit` needs to know where the repo ends and the subpath begins. Hardcoded forges (`github.com`, `gitlab.com`, `bitbucket.org`) split at segment 3 (`host/owner/repo`). For other hosts, an import under a repo already pinned in `BUILD.bit.lock` uses that repo without contacting the host. Otherwise `bit` probes via `git ls-remote` from the longest prefix down — the first one that responds is the repo, the rest is the subpath.

Use `as <ident>` to override the auto-derived provider name — useful when two imports would otherwise collide on the last path segment.

Git URLs are always cloned as `https://<url>`; for SSH or custom auth, add `insteadOf` rules to your `~/.gitconfig` (`bit` shells out to `git clone`, which respects them). A trailing `.git` is optional and stripped from the lock key.

##### Recursive resolution and `BUILD.bit.lock`

Every project — the root and every imported project — has its own `BUILD.bit` listing **its** direct imports, and its own `BUILD.bit.lock` pinning those git deps. Resolution walks the graph: root's imports first, then each imported project's `BUILD.bit` imports, transitively.

- The **root** project's lock is writable: new git imports are auto-pinned to the current default-branch HEAD, and `bit --update` re-resolves entries.
- **Child** projects' locks are read-only. Every git import a child declares **must** have a matching entry in that child's `BUILD.bit.lock`, or `bit` errors. A child with no `BUILD.bit` is a leaf — no further deps to resolve.
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

## CI workflows

`bit --ci <template> [target...]` renders a CI configuration that applies the
same blocks as `bit [target...]`. It splits the work so that
[`large`](#blocks) blocks run in their own jobs:

- Each large block gets a job. Blocks that depend on it run in that job too.
- Large blocks share a job when one depends on another, or when a block
  depends on both.
- Every other block runs in a job named `default`.

Each job runs `bit` with the blocks that nothing else in the job depends on,
such as `bit test clippy`, and `bit` runs everything they depend on. A matrix
block whose slices all run in the job is selected by its name. A
parameterized block is selected with its arguments, as in `bit publish
name=web`, leaving out arguments that equal their default. That selects every slice of a parameterized matrix block, so its
slices share a job. `--ci` does not accept `-P`, because the jobs would run
without it.

Jobs share no results. A block that a large block needs runs again in the
large block's job, and `pre` blocks run in every job. A `post` block depends on
every block, so it puts every large block in one job.

`--ci github` renders the built-in GitHub Actions template. Each job checks out
the repository and runs `bit` in the project directory, and pushes to the
remote's default branch and pull requests trigger the workflow. `bit` must be
on the `PATH`. Regenerate the workflow after changing `BUILD.bit`.

For anything else, such as another CI system, setup steps, or a larger runner
for large jobs, give the path of a [MiniJinja](https://docs.rs/minijinja)
template instead. A name containing `/` is a path, so write `./ci.yml.tmpl`
for a file in the current directory:

```yaml
# buildkite.yml.tmpl, rendered with `bit --ci ./buildkite.yml.tmpl`
steps:
{% for job in jobs %}
  - label: {{ job.name | tojson }}
    key: {{ job.id | tojson }}
    command: {{ job.command | tojson }}
{% endfor %}
```

A template sees:

| Variable            | Description                                                                |
| ------------------- | -------------------------------------------------------------------------- |
| `jobs`              | The jobs, in order                                                         |
| `job.id`            | An identifier made of letters, digits, `_` and `-`, unique among the jobs  |
| `job.name`          | The job's large blocks, or `default`                                       |
| `job.large`         | Whether the job runs large blocks                                          |
| `job.command`       | The shell command the job runs, such as `bit test clippy`                  |
| `job.selection`     | The arguments of `bit` in that command, as a list                          |
| `command`           | The `bit --ci` command that rendered the template                          |
| `working_directory` | The project directory relative to the repository root, or none at the root |
| `default_branch`    | The remote's default branch, or none if unknown                            |

Block tags on their own line leave no blank line, and using an undefined
variable is an error. `tojson` quotes a value for YAML or JSON.

## Providers

### buf

**`buf.generate`** (build) — Generate code from Protobuf files with buf generate. The sources, outputs, and plugins are discovered from the buf workspace and generation template. A remote plugin without a version, such as `buf.build/protocolbuffers/go`, does not rerun on a new release, so pin it, as in `buf.build/protocolbuffers/go:v1.36.6`.

```bit
block = buf.generate {
  input = string?     # Buf input, relative to `dir` (defaults to the workspace in `dir`)
  template = string?  # Generation template path relative to `dir`, or inline YAML or JSON (defaults to "buf.gen.yaml")
  flags = [string]?   # Extra flags passed to buf generate. A config file `--config` names is tracked as an input.
}
```

**Outputs:**

| Field   | Type       | Description                                             |
| ------- | ---------- | ------------------------------------------------------- |
| `paths` | `[string]` | Output paths named by the generation template's plugins |

**`buf.lint`** (test) — Lint Protobuf files with buf lint

```bit
block = buf.lint {
  input = string?    # Buf input, relative to `dir` (defaults to the workspace in `dir`)
  flags = [string]?  # Extra flags passed to buf lint. A config file `--config` names is tracked as an input.
}
```

**Outputs:**

| Field    | Type   | Description            |
| -------- | ------ | ---------------------- |
| `passed` | `bool` | Whether linting passed |

**`buf.format`** (build) — Format Protobuf files with buf format

```bit
block = buf.format {
  input = string?    # Buf input, relative to `dir` (defaults to the workspace in `dir`)
  flags = [string]?  # Extra flags passed to buf format. A config file `--config` names is tracked as an input.
}
```

**`buf.format-check`** (test) — Format Protobuf files with buf format

```bit
block = buf.format-check {
  input = string?    # Buf input, relative to `dir` (defaults to the workspace in `dir`)
  flags = [string]?  # Extra flags passed to buf format. A config file `--config` names is tracked as an input.
}
```

**Outputs:**

| Field    | Type   | Description                     |
| -------- | ------ | ------------------------------- |
| `passed` | `bool` | Whether all files are formatted |

**`buf.breaking`** (test) — Check Protobuf files for breaking changes with buf breaking

```bit
block = buf.breaking {
  input = string?    # Buf input, relative to `dir` (defaults to the workspace in `dir`)
  against = string   # Baseline input to compare against (e.g. ".git#branch=main")
  flags = [string]?  # Extra flags passed to buf breaking. A config file `--config` names is tracked as an input.
}
```

**Outputs:**

| Field    | Type   | Description                            |
| -------- | ------ | -------------------------------------- |
| `passed` | `bool` | Whether no breaking changes were found |

### docker

**`docker.image`** (build) — Build a Docker image (auto-detects inputs from Dockerfile). Changes to a base image tag, a file `ADD` downloads, or a `# syntax=` frontend are not detected, so pin them by digest.

```bit
block = docker.image {
  tag = string                     # Image tag
  context = string?                # Build context directory, relative to `dir`
  dockerfile = string?             # Dockerfile path, relative to `context`
  build_args = {string = string}?  # Docker build arguments
  platform = (string | [string])?  # Target platform(s)
}
```

**Outputs:**

| Field      | Type     | Description                                                          |
| ---------- | -------- | -------------------------------------------------------------------- |
| `ref`      | `string` | Locally pinned tag or registry digest reference                      |
| `image_id` | `string` | Docker image ID or multi-platform manifest digest, without `sha256:` |

**`docker.push`** (build) — Push a Docker image to a registry. Pushes again only when the image changes, not when the registry copy was deleted or overwritten.

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
  volumes = [string]?                   # Volume mounts; relative host paths are relative to `dir`
  environment = {string = string}?      # Environment variables set inside the container. The block's `env` configures the docker CLI instead.
  command = string?                     # Override CMD
  entrypoint = string?                  # Override ENTRYPOINT
  restart = string?                     # Restart policy
  network = string?                     # Docker network
  working_dir = string?                 # Working directory
  healthcheck = (string | {test = string, interval = duration?, timeout = duration?, retries = number?, start_period = duration?})?  # Health check command or config
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
  command = string                # Shell command, run with `bash -eo pipefail`. `BIT_CHANGED_FILES` lists the inputs changed since the last success, one absolute path per line, and is unset on a full run
  outputs = (string | [string])?  # Output file or list of output files, relative to `dir`. They are stored in the shared cache, unless `resolve` or `values` is set
  inputs = [string]?              # Input file glob patterns, relative to `dir`. A directory names every file beneath it
  clean = string?                 # Shell command to run on `bit --clean` (replaces the default removal of outputs)
  resolve = string?               # Shell command whose stdout is captured as state. Used to detect whether the resource exists and whether it has drifted.
  values = string?                # Shell command whose stdout is parsed as a JSON object. Each key becomes a block output.
}
```

**Outputs:**

| Field   | Type        | Description                        |
| ------- | ----------- | ---------------------------------- |
| `path`  | `string?`   | Output path (single-output blocks) |
| `paths` | `[string]?` | Output paths (multi-output blocks) |

**`exec.test`** (test) — Run a command as a test (pass/fail by exit code and any failures reported via `results`)

```bit
block = exec.test {
  command = string                      # Shell command, run with `bash -eo pipefail`. `BIT_CHANGED_FILES` lists the inputs changed since the last success, one absolute path per line, and is unset on a full run
  inputs = [string]?                    # Input file glob patterns, relative to `dir`. A directory names every file beneath it
  outputs = [string]?                   # Output files to track, relative to `dir`
  clean = string?                       # Shell command to run on `bit --clean`
  results = string?                     # jq filter mapping the command's JSON stdout to test results. Each JSON document or JSONL line is one filter input; other stdout lines are logs. Each filter output is `{suite?, name?, event?, status?, duration?, output?}`, about a test if it has a `name` and otherwise about its suite. `event` is `start` or `end`, and `status` (`pass`, `fail`, or `skip`) also ends the test or suite. `duration` is in seconds; without it, the time since `start` is used. A suite is summarized when it ends, or when the command exits.
  report = ("suites" | "tests" | "verbose")?  # What to show from `results`: a line per suite (`suites`), a line per test (`tests`), or a line per test plus logs as they arrive (`verbose`). Outside `verbose`, logs are shown only if the test fails.
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
  package = string    # Go package to build (e.g. "./cmd/myapp")
  output = string?    # Output binary path, relative to `dir` (defaults to the name `go build` gives it: the last element of the package's import path)
  flags = [string]?   # Extra flags passed to go build, except `-o`, which `output` sets. The files `-modfile`, `-overlay` and `-pgo` name are tracked as inputs. go runs in the directory `-C` names.
  inputs = [string]?  # Input file glob patterns, relative to `dir` (in addition to Go sources)
  goos = string?      # Target OS
  goarch = string?    # Target architecture
  cgo = bool?         # Enable cgo
}
```

**Outputs:**

| Field  | Type     | Description              |
| ------ | -------- | ------------------------ |
| `path` | `string` | Path to the built binary |

**`go.build`** (build) — Compile Go packages without producing a binary

```bit
block = go.build {
  package = string | [string]  # Go package pattern or list of patterns (e.g. "./...")
  flags = [string]?            # Extra flags passed to go build. The files `-o` writes are tracked as outputs, and the files `-modfile`, `-overlay` and `-pgo` name as inputs. go runs in the directory `-C` names.
  inputs = [string]?           # Input file glob patterns, relative to `dir` (in addition to Go sources)
  goos = string?               # Target OS
  goarch = string?             # Target architecture
  cgo = bool?                  # Enable cgo
}
```

**`go.generate`** (build) — Run go generate

```bit
block = go.generate {
  package = string | [string]  # Go package pattern or list of patterns (e.g. "./...")
  flags = [string]?            # Extra flags passed to go generate. The files `-modfile`, `-overlay` and `-pgo` name are tracked as inputs. go runs in the directory `-C` names.
  inputs = [string]?           # Input file glob patterns, relative to `dir` (in addition to Go sources)
  outputs = [string]?          # Output file paths produced by generate commands, relative to `dir`
  goos = string?               # Target OS
  goarch = string?             # Target architecture
  cgo = bool?                  # Enable cgo
}
```

**`go.test`** (test) — Run Go tests

```bit
block = go.test {
  package = string | [string]           # Go package pattern or list of patterns (e.g. "./...")
  flags = [string]?                     # Extra flags passed to go test. The profiles `-coverprofile`, `-cpuprofile`, `-memprofile`, `-blockprofile`, `-mutexprofile` and `-trace` write, under `-outputdir`, are tracked as outputs, and the files `-modfile`, `-overlay` and `-pgo` name as inputs. go runs in the directory `-C` names.
  inputs = [string]?                    # Input file glob patterns, relative to `dir` (in addition to Go sources, and every file in each tested package's directory and its `testdata/`). Add files tests read from elsewhere
  report = ("suites" | "tests" | "verbose")?  # What to show: a line per package (`suites`), a line per test (`tests`), or a line per test plus go's output as it arrives (`verbose`). Outside `verbose`, go's output is shown only on failure.
  goos = string?                        # Target OS
  goarch = string?                      # Target architecture
  cgo = bool?                           # Enable cgo
}
```

**Outputs:**

| Field    | Type   | Description              |
| -------- | ------ | ------------------------ |
| `passed` | `bool` | Whether all tests passed |

**`go.lint`** (test) — Run golangci-lint. Defaults to `concurrency = 1`, because golangci-lint refuses to start while another instance is running.

```bit
block = go.lint {
  package = (string | [string])?  # Go package pattern or list of patterns
  flags = [string]?               # Extra flags passed to golangci-lint run. The config `--config` names is tracked as an input instead of the one golangci-lint would find.
  inputs = [string]?              # Input file glob patterns, relative to `dir` (in addition to Go sources)
}
```

**Outputs:**

| Field    | Type   | Description            |
| -------- | ------ | ---------------------- |
| `passed` | `bool` | Whether linting passed |

**`go.fmt`** (build) — Format Go source files

```bit
block = go.fmt {
  package = (string | [string])?  # Go package pattern or list of patterns
}
```

**`go.fmt-l`** (test) — Format Go source files

```bit
block = go.fmt-l {
  package = (string | [string])?  # Go package pattern or list of patterns
}
```

**Outputs:**

| Field    | Type   | Description                     |
| -------- | ------ | ------------------------------- |
| `passed` | `bool` | Whether all files are formatted |

**`go.packages(pattern: string, dir: string?) -> [string]`** — List Go packages matching a package pattern.

**`go.modules(dir: string?) -> [string]`** — List Go modules at or below a directory. Returns each `go.mod` directory relative to `bit`'s working directory, skipping `vendor`, `testdata`, and directories starting with `.` or `_`, as `./...` does.

### pnpm

**`pnpm.install`** (build) — Install the dependencies of the pnpm workspace in `dir`.

```bit
block = pnpm.install {
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
  script = string                 # Script name from `package.json` (e.g. "build")
  package = string?               # Package name from its `package.json`. Omit to run the script of the project in `dir`; at the workspace root, the script may touch any package, so every project's sources are tracked.
  args = [string]?                # Additional arguments passed to the script after `--`
  outputs = (string | [string])?  # Output file or list of output files/directories produced by the script, relative to `dir`
  inputs = [string]?              # Extra input file globs, relative to `dir` (added to the files at the workspace root, and the sources of the package and of its `workspace:`, `link:` and `file:` dependencies)
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
  script = string?    # Script name from `package.json` (defaults to "test")
  package = string?   # Package name from its `package.json`. Omit to run the script of the project in `dir`; at the workspace root, the script may touch any package, so every project's sources are tracked.
  args = [string]?    # Additional arguments passed to the script after `--`
  inputs = [string]?  # Extra input file globs, relative to `dir` (added to the files at the workspace root, and the sources of the package and of its `workspace:`, `link:` and `file:` dependencies)
}
```

**Outputs:**

| Field    | Type   | Description                          |
| -------- | ------ | ------------------------------------ |
| `passed` | `bool` | Whether the test command exited zero |

**`pnpm.packages_with_script(script: string, dir: string?) -> [string]`** — List workspace packages that define a script.

### protoc

**`protoc.generate`** (build) — Generate code from Protobuf files with protoc

```bit
block = protoc.generate {
  sources = [string]        # Protobuf files or glob patterns to compile, relative to `dir`
  include = [string]?       # Import paths passed as `-I` (defaults to `dir`)
  out = {string = string}   # Output directory for each generator (e.g. `{ go = "gen/go" }` passes `--go_out=gen/go`), optionally preceded by generator options and `:`, as in `paths=source_relative:gen/go`
  opt = {string = string}?  # Options for each generator (e.g. `{ go = "paths=source_relative" }` passes `--go_opt=paths=source_relative`). Options that name an existing file are tracked as inputs.
  flags = [string]?         # Extra flags passed to protoc. `-I`/`--proto_path` paths are searched for imports after `include`, `--descriptor_set_in` files are tracked as inputs, and `--descriptor_set_out`/`-o` files as outputs.
  incremental = bool?       # Compile only the sources that changed or import a changed file. Every generator must write each output from a single source; descriptor sets, documentation, and merged API specs need full runs.
}
```

**Outputs:**

| Field   | Type       | Description                  |
| ------- | ---------- | ---------------------------- |
| `paths` | `[string]` | Generator output directories |

### rust

**`rust.build`** (build) — Compile Rust packages. A result shared from another worktree restores nothing into `target/`, so use `rust.exe` for a binary other blocks need.

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
  package = string?                     # Package to test (-p flag)
  flags = [string]?                     # Extra flags passed to cargo test
  report = ("suites" | "tests" | "verbose")?  # What to show: a line per test binary (`suites`), a line per test (`tests`), or a line per test plus cargo's output as it arrives (`verbose`). Outside `verbose`, cargo's diagnostics are shown only on failure.
  features = [string]?                  # Features to enable
  all_features = bool?                  # Enable all features
  target = string?                      # Target triple (e.g. "x86_64-unknown-linux-musl")
  profile = string?                     # Build profile (e.g. "release")
  toolchain = string?                   # Rust toolchain (e.g. "nightly")
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

1. Parse `BUILD.bit` and build a dependency DAG. Module instances expand into
   namespaced blocks, and `pre`/`post` phases add ordering edges.
2. For each block in dependency order, running up to `-j N` at once (defaults
   to the CPU count):
   - Evaluate its fields, with upstream outputs in scope.
   - Ask the provider for the block's input files, its outputs, and the tools
     and environment variables its result depends on.
   - Hash those together with the hashes of the block's dependencies.
   - Skip the block if the hash is unchanged, otherwise apply it.
3. Save each block's state in the user's cache directory.

So editing a source file, deleting an output, upgrading a tool or switching
toolchain reruns exactly the blocks affected. Where a provider cannot see an
input, its schema says so; declare the input in `inputs` or pin it.

### Messages

A command's output is collapsed when its block succeeds. To keep a line
visible, print it to stdout as a message, using the same syntax as GitHub
Actions workflow commands:

```sh
echo "::success::released v$version"
echo "::info::published to crates.io"
echo "::warning::CHANGELOG has no entry for v$version"
echo "::error::tag v$version already exists"
```

`::notice::` is an alias for `::info::`. GitHub parameters such as
`::warning file=src/main.rs,line=12::...` are accepted but not shown. Write
`%0A` for a newline and `%25` for a literal `%`.

Messages appear as soon as they are printed, and stay on screen after the
block finishes. A block that succeeds after printing messages ends with them
instead of "ok". Messages don't affect whether a block passes; only its exit
status does. Skipped and cached blocks don't run their command, so they
don't repeat its messages.

### Shared build cache

Successful builds, tests and lint runs are also recorded in a shared cache, so
another checkout with the same sources, dependencies and toolchain reuses them,
even after the checkout that produced them is deleted. A test or lint result
lets the other checkout skip the run. A build that produces files also stores
them, and the other checkout restores them instead of rebuilding; `bit --plan`
shows this as `⇣`. Failed runs are never shared, and neither are blocks that
act outside the checkout, such as deploys and containers.

Cached results that no run has used for five days are removed from the disk
cache automatically.

To keep a particular output out of the cache, name it in `uncached`:

```hcl
build = exec {
  command  = "pnpm build"
  outputs  = ["dist/", "node_modules/"]
  uncached = ["node_modules/"]
}
```

The block still caches `dist/`. Excluding an output does not change when the
block is considered up to date, only what is stored, so a worktree that reuses
this result gets `dist/` and no `node_modules/`. Exclude an output when it is
large and reproducible by other means, not when later blocks need it.

Checkouts share an entry when the action is the same in each, including the
paths of its files relative to the repository root. Restored files are independent copies, so
editing or deleting one cannot affect the cache. `bit --clean` removes only
the worktree's own outputs and state.

The cache lives under `~/Library/Caches/bit` (or `~/.cache/bit` on Linux).
`--cache` or `BIT_CACHE` chooses another:

- `disk:///path` keeps it in another directory.
- `s3://bucket/prefix` also reads results from an S3 bucket, or any
  S3-compatible service, so other machines such as CI can share them. The
  local cache stays in front of it.
- `s3+rw://bucket/prefix` also publishes results to the bucket. Anyone who can
  write to the bucket decides what everyone else restores, so usually only CI
  should.

Credentials, region, and endpoint come from the usual `AWS_*` variables and
`~/.aws` files, including `AWS_PROFILE`, `credential_process`, and
`AWS_ENDPOINT_URL`. Grant `s3:ListBucket` as well as read access, or S3
reports a result that isn't there as access denied rather than missing. If
the bucket can't be reached, `bit` warns once and builds without it.

`bit --cache <uri> --clean` deletes everything in that cache, for every
project. `bit` does not delete S3 caches; use the bucket's expiry rules.
