# bit — Build It

## Specification v0.1

bit models the software lifecycle as a dependency graph of **blocks**. Each block has a provider, inputs, and outputs. The same syntax describes a compiled binary, a test suite, a Docker image, and a running container.

Providers implement a Rust trait and are compiled into bit. Modules written in bit itself (§2) are the other extension mechanism.

---

## 1. Blocks

The block is the main structural element:

```
name = provider.resource {
  field = value
}
```

A resource named after its provider can be called bare: `exec { ... }` is `exec.exec { ... }`. Fields may be separated by newlines or commas.

Blocks may be prefixed with modifiers:

```
pre setup = exec { ... }              # run before default-phase blocks
post report = exec { ... }            # run after default-phase blocks
protected prod_db = docker.container { ... }
explicit migrate = exec { ... }
large image = docker.image { ... }
post protected explicit large x = exec { ... }
```

`pre` or `post` must come first. `protected`, `explicit`, and `large` may follow in any order. Repeating a modifier is an error. §10 describes what each modifier does.

References create implicit dependency edges:

```
server = go.exe {
  package = "./cmd/server"
}

image = docker.image {
  tag        = "myapp:#{git_sha}"
  build_args = { BINARY = server.path }
}
```

For ordering with content coupling (parent changes invalidate child):

```
container = docker.container {
  image      = image.ref
  name       = "app"
  depends_on = [migrations]
}
```

For ordering without content coupling (parent runs first, but changes don't invalidate child):

```
container = docker.container {
  image = image.ref
  name  = "app"
  after = [migrations]
}
```

`depends_on` means "I depend on this": the parent's content hash is part of the child's. `after` means "run this first": ordering only. Expression refs (e.g. `image.ref`) implicitly create content edges.

Every block also accepts these engine fields, handled by the engine rather than the provider:

| Field | Meaning |
|-------|---------|
| `depends_on` | Content dependencies |
| `after` | Ordering-only dependencies |
| `concurrency` | Maximum number of this block's matrix slices that run at once |
| `dir` | Directory the block runs in; relative paths resolve against it |
| `uncached` | Outputs kept out of the shared build cache |

### Parameterized Blocks

A block can declare parameters. It is instantiated by calling it with named arguments, from a target or an expression:

```
publish(name: string, profile = "release") = exec {
  command = "echo #{name} #{profile}"
}

target release = [publish(name = "app")]
let path = publish(name = "app").path
```

Defaults may refer to earlier parameters. A call with named arguments is a block call. A call with positional arguments is a function call.

### Matrix Expansion

Blocks can expand over list values using `name[key]` syntax:

```
let arch = ["amd64", "arm64"]

binary[arch] = go.exe {
  package = "./cmd/server"
  goarch  = arch
}
```

This creates one block per list element, named `binary["amd64"]` and `binary["arm64"]`. Inside the block, `arch` is a scalar bound to the current element. On the command line, identifier keys may be unquoted: `bit 'binary[arm64]'`.

Blocks with exactly the same key names, in the same order, align. References between them resolve to the matching slice:

```
image[arch] = docker.image {
  tag      = "myapp-#{arch}:latest"
  platform = "linux/#{arch}"
}

container[arch] = docker.container {
  image = image.ref    # resolves to the matching arch slice
  name  = "app-#{arch}"
}
```

Multiple keys produce a cartesian product:

```
let arch = ["amd64", "arm64"]
let os   = ["linux", "darwin"]

binary[arch, os] = go.exe {
  package = "./cmd/server"
  goarch  = arch
  goos    = os
}
# Creates: binary["amd64", "linux"], binary["amd64", "darwin"], binary["arm64", "linux"], binary["arm64", "darwin"]
```

A key can be computed from earlier keys with `key = expr`. The expression is evaluated once per combination of the keys before it:

```
let module = go.modules()

tests[module, package = go.packages("./...", module)] = go.test {
  dir     = module
  package = package
}
```

A specific slice is referenced with key expressions: `binary["arm64", "linux"].path`. Matrix blocks may also take parameters: `build[arch](tag: string) = ...`.

Non-matrix blocks can depend on matrix blocks. The dependency applies to all slices:

```
release = exec {
  command    = "publish"
  depends_on = [binary]   # waits for every slice
}
```

State is keyed per slice by its full name.

## 2. Modules

A `.bit` file is itself a provider. It declares parameters, contains `let` bindings and blocks, and exposes outputs and targets. Expressions outside the module can reach only its declared outputs and targets.

Modules are made available through `import` statements in a project's `BUILD.bit`. Imports in a module's resource files are ignored. **Each import brings exactly one provider into scope.** Resources are `<resource>.bit` files at the root of the imported directory, called as `provider.resource`. A `<provider>.bit` at the root is the default resource, callable bare. An imported module with the same name as a built-in provider shadows it for the resources the module defines.

```
import "./.bit/modules/app"  # provider "app"

# .bit/modules/app/app.bit
param environment = "dev"     # type inferred from the default
param registry    : string
param git_sha     : string

server = go.exe { package = "./cmd/server" }

image = docker.image {
  tag        = "#{registry}/myapp:#{git_sha}"
  build_args = { BINARY = server.path }
}

container = docker.container {
  image = image.ref
  name  = "myapp-#{environment}"
  ports = ["8080:8080"]
}

output image_ref = image.ref
output name      = container.name

target run = [container]
```

```
# BUILD.bit
import "./.bit/modules/app"

let git_sha = exec("git rev-parse --short HEAD") | trim

app = app {
  environment = "staging"
  registry    = "registry.example.com"
  git_sha     = git_sha
}

# Expressions can reference app.image_ref, app.name, and app.run.
# app.server.path is an error: inner blocks are not in the instance's value.

target run = [app.run]
```

Parameters without a default are required. Argument values are type-checked against the parameter types. Outputs are untyped.

Inner blocks become graph nodes named `<instance>.<block>`, such as `app.server`. Expressions cannot reach them, but a target list or the command line can name them directly.

Modules nest arbitrarily. Two instances of the same module produce independent subgraphs. Depending on an instance (`depends_on = [app]`) depends on every block inside it. `depends_on` and `after` on an instance add edges to every inner block, unless the module declares a parameter with that name. Modules may contain parameterized blocks and parameterized targets.

Modifiers on an instance describe it as a whole, so every block it expands to inherits them, including nested instances. `protected`, `explicit`, and `large` are added to each block's own. A `pre` or `post` instance puts its blocks in that phase. Inside a default-phase instance, blocks keep their own phase. A block whose phase contradicts its instance's, such as a `post` block in a `pre` instance, is an error.

A module's leading doc comment becomes its description in `bit --schema`.

### 2.1 Import Resolution

`import` accepts a relative local path (starting with `./` or `../`) or a bare git URL of the form `host/path` (à la Go module paths). Schemes (`https://`, `ssh://`, `file://`), SSH shorthand (`git@host:`), and absolute paths are rejected. The URL is always cloned as `https://<url>`; for SSH or auth rewrites, configure `insteadOf` rules in the user's `~/.gitconfig` — bit shells out to `git clone`, which respects them. A trailing `.git` is optional and stripped for the lock key.

An optional `as <identifier>` clause overrides the auto-derived provider name:

```
import "github.com/alecthomas/bit-modules" as bm
```

The provider name an import contributes is the alias when present, otherwise the last segment of the path:

| Import | Provider |
|---|---|
| `./.bit/modules/docker` | `docker` |
| `../shared/aws` | `aws` |
| `github.com/alecthomas/bit-modules` | `bit-modules` |
| `github.com/alecthomas/bit-modules/aws` | `aws` |
| `git.sr.ht/~user/repo` | `repo` |

Provider names share one namespace across the whole import graph. Two different imports producing the same provider name are a hard error, as is importing the same path under two different names. An identical repeated import is de-duplicated.

Hosts must contain a `.`, and path segments may contain only `A-Z a-z 0-9 - _ . ~`. A local import must name an existing directory.

For git imports with deep paths (e.g. `github.com/foo/bar/waz`), bit needs to know the boundary between repo and subpath. Hardcoded forges (`github.com`, `gitlab.com`, `bitbucket.org`) split at segment 3 and require at least `host/owner/repo`. For other hosts, the longest repo in the project's lock that the import lies under is the boundary, without contacting the host. Unlocked imports are probed with `git ls-remote`, from the longest prefix down. The first prefix that responds is the repo, and the remainder is the subpath. Probes are cached per run by import URL.

### 2.2 Recursive resolution and `BUILD.bit.lock`

Every project, the root and every imported project, may have its own `BUILD.bit` listing its direct imports and its own `BUILD.bit.lock` pinning those git deps. For a git import, the project is the repo root, even when the import names a subpath. For a local import, the project is the imported directory, and its local imports resolve relative to it. Resolution is breadth-first: the root's imports first, then each imported project's imports, transitively.

- The **root** project's lock is writable: new git imports are auto-pinned to the current default-branch HEAD, and `bit --update` re-resolves entries.
- **Child** projects' locks are read-only. Every git import a child declares **must** have a matching entry in that child's `BUILD.bit.lock`, or bit errors. A child with no `BUILD.bit` is a leaf — no further deps to resolve.
- Imports of imports become providers in the root project's scope, so any module can use any transitively imported provider.
- The same git repo resolved to two different SHAs anywhere in the graph = hard error.

Lock file format (commit to version control):

```toml
"github.com/alecthomas/bit-modules" = "a1b2c3d4e5f6789..."
```

The key is the repo boundary (`host/owner/repo`) with any trailing `.git` or `/` stripped, so several subpath imports of one repo share an entry. The value is a full commit SHA. Local-path imports are not locked. The root lock is rebuilt on every run, so entries for removed imports disappear. It is written only when it changes, and deleted when empty.

A normal `bit` run honours the root lock. It contacts the network to:

- pin a new direct import to its default branch HEAD (`git ls-remote --symref <url> HEAD`);
- fetch a pinned commit that isn't in the local cache yet;
- probe the repo boundary of an unlocked import from a host that isn't a hardcoded forge.

`bit --update [<repo>...]` re-resolves matching root entries against their default branch, prints each change, rewrites the root lock, and exits without building. A filter matches a lock key, with optional `.git` or trailing `/`, or a subpath URL under it. A filter that matches nothing produces a warning. Child locks aren't touched.

Bare clones live in `<cache dir>/bit/git/<key>.git`. Each pinned commit is extracted into an immutable directory, `<cache dir>/bit/modules/<key>/<sha>`, through a temporary directory and a rename so concurrent runs don't collide.

## 3. Expression Language

Intentionally minimal. No loops, no user-defined functions. `if` expressions are supported.

### Types

| Type | Notes |
|------|-------|
| `string` | |
| `number` | `int` is an alias |
| `bool` | |
| `duration` | |
| `path` | |
| `secret` | |
| `[T]` | List of `T` |
| `{string = T}` | Map with values of type `T` |
| `A \| B` | Union, e.g. `string \| [string]` |

### Literals

- **Strings** — Double-quoted strings support the escapes `\n`, `\r`, `\t`, `\"`, `\\`, and `\#`, and `#{expr}` interpolation. Any other backslash sequence is kept as written. Single-quoted strings are raw: no escapes and no interpolation.
- **Heredocs** — `<<LABEL` keeps indentation, and `<<-LABEL` strips the common indent. Both support `#{expr}`.
- **Numbers** — Arbitrary-precision decimals.
- **Durations** — Unquoted, a number fused with a unit suffix: `5s`, `500ms`, `1.5h`, `30m`, `1d`. Units are `ns`, `us`, `ms`, `s`, `m`, `h`, `d`. A bare `5` is a number.
- **Lists** — `[a, b]`.
- **Maps** — `{ key = value }`. Keys are identifiers or strings. A key may carry a type (`{ age: number = null }`), and a `null` value makes the field optional.
- **`true`, `false`, `null`**.

Lists, maps, argument lists, and block fields allow trailing commas. Identifiers may contain `-`.

### Comments

`#` starts a comment and must be followed by whitespace or the end of the line. `#{` starts interpolation, and other `#x` forms are reserved. A comment directly above a statement is its doc comment, and a blank line detaches it. A comment block at the top of a file is the module's doc comment.

### Operators

From loosest to tightest binding:

| Operator | Meaning |
|----------|---------|
| `if c then a else b` | `c` must be a bool |
| `a + b` | Add numbers, concatenate strings or lists |
| `a \| f(args)` | Pipe |
| `a == b`, `a != b` | Equality |

So `a + b | trim` is `a + (b | trim)`.

### Built-in Functions

| Function | Description |
|----------|-------------|
| `env(name, default?)` | Read an environment variable, or return `default` (any type) if unset |
| `exec(command)` | Run a shell command and return stdout |
| `glob(pattern)` | Expand a filesystem glob |
| `secret(name)` | Read a secret by name, currently from the environment variable `name` |
| `project_dir()` | Absolute directory containing `BUILD.bit` |
| `lines(s)` | Split into nonempty lines |
| `split(s, sep)` | Split by a separator |
| `trim(v)` | Trim whitespace |
| `trim_prefix(v, text)`, `trim_suffix(v, text)` | Remove a prefix or suffix, if present |
| `prefix(v, text)`, `suffix(v, text)` | Prepend or append text |
| `basename(v)`, `dirname(v)` | File name or directory of a path |
| `replace(v, pattern, replacement)` | Replace regular expression matches. `replacement` may use `$1` or `${name}` |
| `filter(list, pattern)` | Keep strings matching a regular expression |
| `uniq(list)` | Deduplicate, preserving order |
| `sha256(s)` | SHA-256 of a string |
| `shell_join(v)` | Quote as POSIX shell words joined by spaces |
| `shell_split(s)` | Split a POSIX shell string into words |

Functions whose argument is `v` accept a string or a list of strings, applied to each element. `bit --schema` lists the exact signatures.

`exec()` runs when the expression is evaluated. In a `let` binding that is at load time. In a block's fields, it runs when that block's inputs are evaluated.

### Pipes

`a | f(b, c)` is exactly `f(a, b, c)`, so every built-in function can be piped, and `f()` can be written `f`. Only built-in functions can be piped, not provider functions.

```
let git_sha = exec("git rev-parse --short HEAD") | trim

let packages = exec("go list -deps -f '{{.Dir}}' ./cmd/server/...")
               | lines
               | uniq
```

### Variables

```
let git_sha = exec("git rev-parse --short HEAD") | trim
let arch : [string] = ["amd64", "arm64"]
```

### If Expressions

```
let replicas = if environment == "production" then 3 else 1
```

### Parameters

```
param environment : string
param replicas    : number = 1   # default makes it optional
param profile     = "release"    # type inferred from the default
param db_password : secret
```

Parameters are set with `-P key=value` on the command line, or by arguments when the file is instantiated as a module.

### Provider Functions

Providers export helper functions, callable in expressions under the provider namespace:

```
let packages = go.packages("./...")
```

## 4. Targets

Named groups for CLI invocation:

```
target build  = [server, image]
target test   = [unit_tests, lint]
target deploy(name: string) = [publish(name = name)]
```

An entry is a block, target, or module member (`app.run`), optionally followed by matrix keys (`binary["arm64"]`) and named arguments. Targets can take parameters, passed on the command line as `bit deploy name=app`. `bit` with no arguments runs the `default` target, or every non-`explicit` block if there is none. `bit ...` runs every non-`explicit` block.

`output name = expr` exports a value from a module (§2).

## 5. Providers

A **provider** implements the `Provider` trait and groups related resources and shared functions. A **resource** implements the `Resource` trait and handles a single block type. For example, the `go` provider contains `go.exe` and `go.test` resources. The core engine has no knowledge of any language, tool, or platform.

Built-in providers:

| Provider | Resources | Functions |
|----------|-----------|-----------|
| `exec` | `exec`, `exec.test` | |
| `docker` | `docker.image`, `docker.push`, `docker.container`, `docker.network`, `docker.network_attach` | |
| `go` | `go.exe`, `go.build`, `go.generate`, `go.test`, `go.lint`, `go.fmt`, `go.fmt-l` | `go.packages`, `go.modules` |
| `rust` | `rust.build`, `rust.exe`, `rust.test`, `rust.clippy`, `rust.fmt`, `rust.fmt-check` | `rust.packages`, `rust.dependencies` |
| `pnpm` | `pnpm.install`, `pnpm.run`, `pnpm.test` | `pnpm.packages_with_script` |
| `buf` | `buf.generate`, `buf.lint`, `buf.format`, `buf.format-check`, `buf.breaking` | |
| `protoc` | `protoc.generate` | |

`bit --schema` prints each resource's fields and outputs.

### 5.1 Resource Kinds

- **`build`** (default) — skipped if its content hash is unchanged.
- **`test`** — skipped like build blocks while passing. A failed test reruns on the next run, is never stored in the shared build cache, and keeps its last successful run as the baseline for detecting changes. A failed test stops the run (§12).

Linters such as `go.lint` and `rust.clippy` are `test` resources.

### 5.2 Provider Functions

Functions exported by the provider, available in expressions under the provider namespace, e.g. `go.modules()`. Utility-only providers (no resources, only functions) are valid.

### 5.3 Resolve

Resources discover the full set of input and output files from minimal user configuration. This is what allows `.bit` files to be terse: the user writes `package = "./cmd/server"` and the resource discovers every contributing source file.

A resource reports three things:

- **`resolve()`** — a map from each source file path to its SHA-256. Providers hash through a shared file tracker, so files used by several blocks are read once.
- **`output_keys()`** — the paths the block produces. A trailing `/` marks a directory.
- **`ambient()`** — tool versions, executables, and environment variables the result depends on. The engine fingerprints them and adds them to the sources under `version:`, `executable:`, and `env:` keys.

For example, the `docker.image` provider parses the Dockerfile to discover `COPY` and `ADD` source paths automatically, skipping multi-stage `COPY --from=...` and `ADD` with URLs.

## 6. Lifecycle

Every block goes through up to four phases: **resolve**, **plan**, **apply**, and **destroy**. The lifecycle is the same for all providers. Whether a provider manages external resources or produces local files is an implementation detail, and the protocol is identical.

### 6.1 Resolve

Resolve discovers source files from the user-supplied configuration (§5.3). For a `go.exe` block where the user writes `package = "./cmd/server"`, resolve follows the Go import graph and returns every file that contributes to the binary. A resource that manages no files, such as `docker.network`, can return an empty map.

Resolve runs on every plan and apply. A file's hash is cached for the rest of the run once its timestamps are at least two seconds old, so files that are still being written are re-read.

### 6.2 Change Detection

For each block the engine computes a **content hash** (§8) from the block's evaluated fields, its sources and outputs, and the content hashes of its `depends_on` parents. The hash is persisted with the block's state. On the next run the engine recomputes it and compares.

Changes propagate through the DAG. If block A's inputs change, A's content hash changes. Block B depends on A, so A's hash is part of B's, and B is invalidated too.

Within a single plan, the engine also keeps a dirty set: if a parent is planned for a change, every descendant is planned for an update, since the parent's new state hasn't been persisted yet. During apply, a child's hash uses the parent's hash from the current run.

### 6.3 Plan

The engine always calls the provider's `plan` with the current inputs and prior state. Plan returns one of `create`, `update`, `destroy`, `restore`, or `none`, with a description and an optional reason. The provider handles domain-specific comparisons, such as a changed command or tag. The engine handles file-level change detection.

The engine upgrades `none` to `update`, with a generated reason, when:

- the content hash differs from the stored one;
- a dependency is dirty in this run;
- the block is a test that failed last time.

`--force` upgrades `none` to `update` or `create` and bypasses the shared cache.

If the block can use the shared build cache (§8.1), the engine then looks for a receipt. A valid receipt skips the block as cached. A receipt whose outputs are missing locally becomes `restore`, which rebuilds them from the cache.

**Protected blocks**: a plan to `destroy` a protected block is an error.

### 6.4 Apply

Apply executes the planned action. It receives the inputs, the prior state, the set of input paths changed since the block last succeeded (or "all"), and a writer for the block's output. It returns `outputs`, which downstream blocks can reference, and an optional `state`. On failure it returns an error.

After apply, the engine re-resolves sources, hashes the declared outputs, and recomputes the content hash. It then persists a wrapped state containing:

- the provider's state;
- the block's outputs;
- the `dir` it ran in;
- the content hash and its parts (input hash, resolved sources, dependency hashes).

Outputs are persisted so that skipped blocks can still provide values to downstream blocks. On the next run, the provider's state is passed back as `prior_state`. If the provider returns no state, nothing is persisted.

**Test blocks** return `passed` (bool). A test that runs and fails returns `passed = false`. An error return means the test could not be run at all.

### 6.5 Destroy

Destroy removes everything a block has produced. The engine calls the provider's `destroy` with the prior state, then removes the persisted state. The provider cleans up, for example by deleting output files or removing a container.

`bit --clean` with no targets destroys every non-`explicit` block, regardless of the `default` target. With targets, it destroys them and everything that depends on them through any edge, including `after` and phase edges. Blocks are destroyed in reverse dependency order. With `--force`, cleaning continues past errors.

`--clean` never touches the shared build cache. `--cache --clean` deletes it.

**Protected blocks** are skipped with a notice unless the block itself is named on the command line. Naming a target that contains it, or using `...`, is not enough.

### 6.6 Rollback

**TBD.** If apply succeeds for a block but a later block fails, nothing rolls back the already-applied blocks. This is a known gap.

### 6.7 Phase Summary

|  | Resolve | Plan | Apply | Destroy |
|--|---------|------|-------|---------|
| **Engine** | Calls resolve, output_keys, and ambient; computes the content hash | Calls plan, upgrades `none` on change, consults the shared cache | Calls apply, re-hashes, persists state, publishes a cache receipt | Calls destroy, removes state |
| **Provider** | Returns source hashes, output paths, ambient dependencies | Compares domain-specific fields | Executes the action, returns outputs and state | Cleans up produced artifacts |
| **Protected** | Normal | `destroy` is an error | Normal | Skipped unless named directly |
| **Test** | Normal | Reruns after a failure | Returns `passed` | Normal |

## 7. The `exec` Provider

`exec` is an ordinary built-in provider for wrapping CLI tools. Its default resource is also named `exec`, so it is called bare.

| Field | Meaning |
|-------|---------|
| `command` | Shell command to run (required) |
| `inputs` | Input globs |
| `outputs` | Output file or directory, or a list of them |
| `clean` | Command run by `--clean` instead of removing the outputs |
| `resolve` | Command whose stdout is stored as state. Plan is `create` if it fails and `update` if its output changed |
| `values` | Command whose stdout is a JSON object. Each key becomes a block output |

```
docs = exec {
  command = "mdbook build docs/ -d book"
  inputs  = ["docs/**/*.md", "book.toml"]
  outputs = "book/"
}
```

`command` runs as `bash -eo pipefail -c <command>` in `dir`. It receives `BIT_CHANGED_FILES`: the absolute paths of inputs changed since the block last succeeded, one per line. The variable is unset on a full run, or when the list exceeds 64 KiB. Changing `command` or `clean` plans an update.

`inputs` is a list of globs expanded by `resolve()`. A directory names every file beneath it. A valid pattern that matches nothing is allowed. Relative `inputs`, `outputs`, and `uncached` paths are relative to `dir`.

A trailing `/` in `outputs` marks a directory, which is created before the command runs. For a file output, its parent directory is created. A block with one output exposes it as `path`. A block with several outputs exposes them as `paths`:

```
build = exec {
  command = "make all"
  inputs  = ["src/**/*.c", "Makefile"]
  outputs = ["bin/server", "bin/cli"]
}
```

Blocks with outputs and no `resolve` or `values` can use the shared build cache. Others are local only.

Dynamic input discovery uses `exec()` in the expression language:

```
server = exec {
  command = "go build -o bin/server ./cmd/server"
  inputs  = ["go.mod", "go.sum"]
            + exec("go list -deps -f '{{.Dir}}/*.go' ./cmd/server/...") | lines
  outputs = "bin/server"
}
```

The `exec()` call runs when the block's inputs are evaluated, before `command` runs. Its output is split into lines and appended to the static globs.

`exec.test` runs a command as a test. It takes `command`, `inputs`, `outputs` (a list), `clean`, `results`, and `report`, and outputs `passed`:

```
unit = exec.test {
  command = "go test -json ./..."
  inputs  = ["**/*.go", "go.mod", "go.sum"]
  results = <<-EOF
    {suite: .Package} + (if .Test then {name: .Test} else {} end) +
      if .Action == "start" or .Action == "run" then {event: "start"}
      elif .Action == "output" and (.Output | test("^(=== |--- )") | not) then {output: .Output}
      elif .Action == "pass" or .Action == "fail" or .Action == "skip" then {status: .Action, duration: .Elapsed}
      else empty end
  EOF
  report  = "tests"
}
```

Without `results`, a non-zero exit means `passed = false`, not an error. `results` is a jq filter applied to each JSON document or JSONL line of stdout. It maps them to `{suite?, name?, event?, status?, duration?, output?}` records. With `results`, the test passes only if the command exits successfully and every reported test and suite passed. `report` selects `suites` (default), `tests`, or `verbose` output (§9).

If an `exec` block becomes complex, that's the signal to write a real provider.

## 8. Caching and Invalidation

### Content Hash (Merkle Tree)

Each block's state includes a **content hash** computed by the engine:

```
content_hash = sha256(
  input_hash,   # sha256 of the block's evaluated fields as canonical JSON, paths project-relative
  resolve_map,  # sources, ambient fingerprints, and existing declared outputs (directories file by file)
  dep_hashes,   # name + content hash of each depends_on parent
)
```

Every field except `concurrency` and `after` is part of `input_hash`. `after` is left out because evaluating it yields the parents' outputs, which would couple the block to their content. A parent's hash comes from the current run if it ran, otherwise from its stored state.

This forms a Merkle tree over the DAG. A change in any leaf propagates up through every block that depends on it.

### Invalidation

- **File changes** — a modified source file changes the content hash of every block that includes it.
- **Field changes** — changing a block's evaluated fields changes its hash.
- **Tool changes** — a changed tool version, executable, or environment variable reported by `ambient()` changes the hash.
- **Parent changes** — a parent's content hash is part of the child's.
- **Output deletion** — output files are included in the hash. If an output is deleted, the block rebuilds.
- **Within-run propagation** — during plan, descendants of a block planned for a change are planned for an update.

### 8.1 Shared Build Cache

Besides the per-project state, bit keeps a build cache shared across projects and git worktrees. It lives in `$BIT_CACHE_DIR`, or `<cache dir>/bit`, and has two parts:

- **Receipts** at `actions/v1/<project-id>/<key>.json`. The project id combines the git common directory with the project's path inside the worktree, so linked worktrees share receipts. Outside git, it is the canonical project path.
- **Artifacts** in a content-addressed store at `cas/v1/sha256/`, shared by all projects.

The action key hashes:

- the receipt version, provider, resource, and the resource's cache version;
- the block name, OS, and architecture;
- the input hash;
- the sources, excluding files that are only outputs;
- the dependencies' result hashes.

A resource opts in through `cache_policy()`, which returns `Local` or `Shared { version }`. The cache is consulted only when the block uses a shared policy, the plan is not `none`, the run isn't forced, and no dependency is dirty. It is never consulted for a test that failed last time.

The lookup key uses sources measured before apply. The publish key uses sources re-measured after apply. Inputs that a resource only discovers after building must therefore also be found before it, or the keys never match.

On a hit, `check_receipt()` decides the result:

- **Valid** — the outputs are already in place, so the block is skipped as cached.
- **Restore** — `materialize()` rebuilds the outputs from the artifact store.
- **Unusable** — the block runs.

The receipt's outputs and state are bound to the block. If binding fails, the block runs.

After a successful apply, the engine captures the outputs with `capture_artifacts()` and publishes a receipt containing state, outputs, artifacts, and the content hash. Publishing is best-effort, and a conflicting receipt produces a warning. Outputs listed in `uncached` are not stored. Failed tests are never published.

## 9. Tests

Tests are blocks whose resource has `kind = "test"`. There is no `test` keyword in the language.

Test resources run their tool and parse its native output into suite and test results: `go test -json` for `go.test`, cargo's output for `rust.test`, or a jq `results` filter for `exec.test`. The `report` field controls what is shown:

- `suites` (default) — a line per suite;
- `tests` — a line per test;
- `verbose` — a line per test, plus the tool's output as it arrives.

Outside `verbose`, the tool's output is shown only when the test fails.

Test blocks output `passed` (bool). `passed = false` stops the run (§12).

## 10. State

When a provider returns `state` from apply, the engine persists it. Providers that return no state have nothing persisted. The engine treats them identically; there is no separate category of "stateful" vs "stateless" provider.

State lives in `<cache dir>/bit/<sha256 of the canonical project root>/state/`. Each block has one JSON file, named by the SHA-256 of its full block name, containing `{block, state}`. Writes are atomic: a temporary file followed by a rename.

### Protected Blocks

The `protected` modifier guards a block against `--clean`. A protected block is skipped with a notice unless it is named directly on the command line. A plan that would destroy it is an error.

```
protected db = docker.container {
  image = "postgres:16"
  name  = "myapp-db"
}
```

### Explicit Blocks

Blocks can be prefixed with the `explicit` modifier to exclude them from the `...` selector (and the no-target/no-`default` fallback) for every action — apply, plan, destroy, dump. An `explicit` block still runs when named directly (`bit migrate`) or pulled in as a transitive dependency of a named target/block. The modifier is independent of `protected` and the two may be combined in either order.

```
explicit migrate = exec {
  command = "./migrate"
  inputs  = ["./schema.sql"]
}

protected explicit prod_db = docker.container {
  image = "postgres:16"
  name  = "myapp-prod-db"
}
```

### Large Blocks

The `large` modifier marks a block as long-running or resource-heavy. Blocks without it are small. The engine records the size on each graph node. Matrix slices and parameterized instances take it from their declaration. Every block that a `large` module instance expands to is also `large`. The modifier does not change local execution and may be combined with `protected` and `explicit` in any order.

```
large image = docker.image {
  context = "."
}
```

## 11. Rust Traits

Abbreviated from `src/value.rs`, `src/schema.rs`, and `src/provider.rs`.

```rust
pub type Map = HashMap<String, Value>;
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub enum Value {
    Null,
    Bool(bool),
    Number(BigDecimal),
    Str(String),
    BlockRef(String),
    Duration(Duration),
    List(Type, Vec<Value>),
    Map(Type, Map),
    Struct(StructType, Map),
}

pub enum Type {
    Any, String, BlockRef, Number, Bool, Duration,
    List(Box<Type>),
    Map(Box<Type>),
    Struct(StructType),
    Optional(Box<Type>),
    Path, Secret,
    Union(Vec<Type>),
    Literal(String),  // provider schemas only; no language syntax
}

pub struct StructField {
    pub typ: Type,
    pub default: Option<Value>,
    pub description: Option<String>,
}

pub struct StructType {
    pub description: Option<String>,
    pub fields: Vec<(String, StructField)>,
}

/// Derived with `#[derive(bit_derive::Schema)]` for resource inputs and outputs.
pub trait Schema {
    fn schema() -> StructType;
}

pub enum ResourceKind { Build, Test }

pub enum PlanAction { Create, Update, Destroy, Restore, None }

pub struct PlanResult {
    pub action: PlanAction,
    pub description: String,
    pub reason: Option<String>,
}

pub struct ApplyResult<S, O> {
    pub outputs: O,
    pub state: Option<S>,
}

/// Input paths changed since the block last succeeded.
pub enum Changes { All, Paths(BTreeSet<PathBuf>) }

pub enum CachePolicy { Local, Shared { version: u32 } }

pub enum ReceiptCheck { Valid, Restore, Unusable }

/// Tool versions, executables, and environment variables a result depends on.
pub enum Ambient {
    Version(VersionCommand),
    Executable(String),
    Variable(String),
    Variables(String),  // every variable with this prefix
}

/// Block inputs or state, with the block's `dir`. Derefs to `T`.
pub struct Block<T> {
    pub dir: Option<String>,
    pub value: T,
}

pub struct FuncSignature {
    pub name: String,
    pub description: Option<String>,
    pub params: Vec<(String, StructField)>,
    pub returns: Type,
}

pub struct ResourceSchema {
    pub kind: ResourceKind,
    pub inputs: StructType,
    pub outputs: StructType,
}

/// A provider groups related resources and shared functions,
/// e.g. "go" contains go.exe, go.test, etc.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn resources(&self) -> Vec<Box<dyn DynResource>>;
    fn functions(&self) -> Vec<FuncSignature>;
    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError>;
}

/// A resource implements a single block type, e.g. "exe" within "go".
pub trait Resource {
    type State: Serialize + DeserializeOwned;
    type Inputs: DeserializeOwned + Schema;
    type Outputs: Serialize + Schema;

    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    fn schema(&self) -> ResourceSchema;  // default: from Inputs and Outputs

    fn resolve(&self, inputs: &Block<Self::Inputs>) -> Result<BTreeMap<String, SHA256>, BoxError>;
    fn ambient(&self, inputs: &Block<Self::Inputs>) -> Vec<Ambient>;  // default: none
    fn output_keys(&self, inputs: &Block<Self::Inputs>) -> Vec<String>;  // default: none

    fn plan(&self, inputs: &Block<Self::Inputs>, prior_state: Option<&Self::State>)
        -> Result<PlanResult, BoxError>;
    fn apply(&self, inputs: &Block<Self::Inputs>, prior_state: Option<&Self::State>,
             changes: &Changes, writer: &BlockWriter)
        -> Result<ApplyResult<Self::State, Self::Outputs>, BoxError>;
    fn destroy(&self, prior_state: &Block<Self::State>, writer: &BlockWriter) -> Result<(), BoxError>;

    fn announces_start(&self) -> bool;  // default: false
    fn default_concurrency(&self) -> Option<usize>;  // default: unlimited

    // Shared build cache contract. The engine owns keys, receipts, and the
    // artifact store; the resource owns what its artifacts mean.
    fn cache_policy(&self, inputs: &Block<Self::Inputs>) -> CachePolicy;
    fn capture_artifacts(&self, inputs, state, outputs: &[OutputFile], cas: &Cas)
        -> Result<BTreeMap<String, ArtifactRef>, BoxError>;  // default: output files
    fn check_receipt(&self, inputs, state, outputs, artifacts, cas)
        -> Result<ReceiptCheck, BoxError>;  // default: compare output files
    fn materialize(&self, inputs, state, outputs, artifacts, cas, writer)
        -> MaterializeResult<Self::State, Self::Outputs>;  // default: restore output files
}
```

`DynResource` is the object-safe form, working on `Map` and `serde_json::Value`. A blanket implementation covers every `Resource + Send + Sync`.

## 12. Execution Model

```
parse .bit files, resolve imports
  ▼
build DAG from references, depends_on, after, and phases
  ▼
select blocks (targets, ..., --since)
  ▼
for each block (topological order, parallel where possible):
  ├─ evaluate fields, validate against the schema
  ├─ resolve sources, ambient, and output paths → compute content hash
  ├─ plan; upgrade none → update if the hash changed or a dependency is dirty
  ├─ none → skip (load stored outputs into scope)
  ├─ shared cache hit → bind receipt, or restore outputs from it
  └─ otherwise:
      ├─ apply → outputs + state
      ├─ re-resolve → recompute content hash
      ├─ persist state + outputs + content hash
      └─ publish a shared cache receipt
```

Blocks run in topological order, with ties broken alphabetically. A ready-queue scheduler runs up to `-j` blocks at once, defaulting to the number of CPUs. `-j1` runs them one at a time.

A block's `concurrency` field caps how many of its matrix slices run at once. A resource can set a default through `default_concurrency()`; `go.lint` defaults to 1.

`pre` blocks run before every default-phase block, and `post` blocks after them, through ordering-only edges.

The first error or failed test stops the run. No new blocks start, including blocks on independent branches. Blocks already running finish and their state is saved. Then bit exits with the error.

`--since <ref>` narrows the selection to blocks affected by changes since `merge-base(ref, HEAD)`, plus the blocks they require. A block is affected when a changed path is one of its inputs, or when it depends on an affected block. bit falls back to wider selections when `.bit` files changed, a block's inputs were never recorded, or blocks were removed.

## 13. CLI

Positional arguments name targets or blocks, followed by `name=value` target arguments. A flag selects the operation; with no flag, bit applies. Operations map to lifecycle phases:

| Invocation | Phases | Description |
|------------|--------|-------------|
| `bit [target...]` | resolve → plan → apply | Apply targets (`default` target if defined, else every non-`explicit` block; `...` for every non-`explicit` block) |
| `bit --plan [target...]` | resolve → plan | Show what would change without applying |
| `bit --clean [target...]` | destroy | Destroy targets and their dependents in reverse dependency order |
| `bit --graph [target...]` | — | Render the dependency graph; with `--plan`, colour nodes by planned action |
| `bit --dump [target...]` | — | Show evaluated inputs and stored outputs |
| `bit --list` | — | List targets; `-ll` lists all blocks as a dependency tree |
| `bit --info` | — | Show parameters, targets, and outputs |
| `bit --schema [name]` | — | Show built-in functions and provider schemas |
| `bit --update [repo...]` | — | Re-resolve git imports and rewrite `BUILD.bit.lock` |
| `bit --fmt [file]` | — | Format `BUILD.bit`, or the named `.bit` file, in place |
| `bit --cache` | — | Show the shared build cache size; with `--clean`, delete it |

Modifiers:

- `--force` (`-f`) — rebuild past change detection and the cache; with `--clean`, continue past errors.
- `--since [ref]` — limit apply, plan, graph, dump, and block listing to blocks affected since `merge-base(ref, HEAD)`.
- `-P key=value` — set a module parameter.
- `-j N` — number of parallel jobs.

## 14. Example: Source to Container

```
# ── .bit/modules/service/service.bit ──
# A Go service: binary, tests, and image.

param name : string
param tag  : string

server = go.exe {
  package = "./cmd/#{name}"
  output  = "bin/#{name}"
  goos    = "linux"
}

tests = go.test { package = "./...", flags = ["-race"] }
lint  = go.lint {}

large image = docker.image {
  tag        = "#{name}:#{tag}"
  build_args = { BINARY = server.path }
}

output image_ref = image.ref

target build = [image]
target test  = [tests, lint]
```

```
# ── BUILD.bit ──

import "./.bit/modules/service"

let git_sha  = exec("git rev-parse --short HEAD") | trim
let registry = env("REGISTRY", "localhost:5000")

api = service {
  name = "api"
  tag  = git_sha
}

# Reading api.image_ref depends on the whole instance, so its tests run first.
push = docker.push {
  image = api.image_ref
  tag   = "#{registry}/api:#{git_sha}"
}

# ── Local environment ──

network = docker.network { name = "myapp" }

protected db = docker.container {
  image       = "postgres:16"
  name        = "myapp-db"
  network     = network.name
  environment = { POSTGRES_DB = "app", POSTGRES_PASSWORD = "dev" }
  healthcheck = { test = "pg_isready", interval = 1s, retries = 10 }
}

app = docker.container {
  image       = api.image_ref
  name        = "myapp-api"
  network     = network.name
  ports       = ["8080:8080"]
  environment = { DATABASE_URL = "postgres://postgres:dev@myapp-db:5432/app" }
  depends_on  = [db]
}

smoke = exec.test {
  command    = "curl -sf --retry 10 --retry-connrefused http://localhost:8080/health"
  depends_on = [app]
}

target default = [api.build]
target test    = [api.test]
target run     = [app, smoke]
target release = [push]
```

`bit` builds the image. `bit test` runs the Go tests and linter. `bit run` builds and tests the service, starts it with its database, then runs a smoke test against it. `bit release` builds and tests the service, then pushes the image. `bit --clean` removes everything bit built except `db`, which must be named directly: `bit --clean db`.
