# bit — Build It

## Specification v0.1

bit models the full software lifecycle — source code to production — as a dependency graph of **blocks**. Each block has a provider, inputs, and outputs. The same syntax describes a compiled binary, a Docker image, a database, and a Kubernetes deployment.

Providers implement a Rust trait. External providers will be supported via WASM in the future.

---

## 1. Blocks

The block is the only structural element:

```
name = provider.resource {
  field = value
}
```

Blocks may be prefixed with modifiers:

```
protected name = provider.resource { ... }
```

References create implicit dependency edges:

```
server = go.binary {
  main = "./cmd/server"
}

image = docker.image {
  tag        = "myapp:${git_sha}"
  build_args = { BINARY = server.path }
}
```

For ordering without data flow:

```
deploy = kubernetes.deployment {
  image      = image.ref
  depends_on = [migrations]
}
```

## 2. Modules

A `.bit` file is itself a provider. It declares parameters, contains blocks, and exposes outputs and targets. Only declared outputs and targets are accessible from outside the module — inner blocks are private.

```
# app.bit
param environment : string
param replicas    : int
param registry    : string
param git_sha     : string

server = go.binary { main = "./cmd/server" }

image = docker.image {
  tag = "${registry}/myapp:${git_sha}"
}

app = kubernetes.deployment {
  image    = image.ref
  replicas = replicas
}

output image_ref = image.ref
output endpoint  = app.endpoint

target deploy = [app]
```

```
# prod.bit
let git_sha = exec("git rev-parse --short HEAD") | trim

app = ./app.bit {
  environment = "production"
  replicas    = 5
  registry    = "registry.example.com"
  git_sha     = git_sha
}

# Can reference: app.image_ref, app.endpoint, app.deploy
# Cannot reference: app.server, app.image, app.app (inner blocks are private)

target deploy = [app.deploy]
```

Modules nest arbitrarily. Two instances of the same module produce independent subgraphs. A module can only be depended on as a whole — use `depends_on = [module_name]` or reference one of its targets.

## 3. Expression Language

Intentionally minimal. No loops, no user-defined functions. `if` expressions are supported.

### Types

`string` (with `${}` interpolation), `int`, `bool`, `list`, `map`, `path`, `secret`.

### Built-in Functions

| Function | Description |
|----------|-------------|
| `env(name, default?)` | Read environment variable |
| `exec(cmd)` | Run command, return stdout as string |
| `secret(name)` | Resolve a named secret |

`secret()` resolves secrets through pluggable secret providers (e.g. environment variables, Vault, AWS Secrets Manager). The secret provider protocol is TBD. Secrets are masked in plan output and logs.
| `glob(pattern)` | Expand filesystem glob to list of paths |

`exec()` evaluates at expression evaluation time — in a `let` binding that's early, but inside a block's fields it runs when that block's inputs are being resolved.

### Pipes

Expressions support pipe syntax for transforming values:

```
let git_sha = exec("git rev-parse --short HEAD") | trim

let packages = exec("go list -deps -f '{{.Dir}}/*.go' ./cmd/server/...")
               | lines
               | uniq
```

| Pipe | Description |
|------|-------------|
| `lines` | Split string into list on newlines, drop empties |
| `trim` | Strip whitespace (on string, or each element of list) |
| `split(sep)` | Split string into list on separator |
| `uniq` | Deduplicate a list |

Pipes chain left to right: `exec("...") | trim | lines | uniq`

### Variables

```
let git_sha = exec("git rev-parse --short HEAD") | trim
```

### If Expressions

```
let replicas = if environment == "production" then 3 else 1
```

### Parameters

```
param environment : string
param replicas    : int = 1     # default makes it optional
param db_password : secret
```

### Provider Functions

Providers export pure helper functions, callable in expressions:

```
annotations = {
  "registry" = docker.registry_host(image.ref)
}
```

## 4. Targets

Named groups for CLI invocation:

```
target build  = [server, image]
target test   = [unit_tests, integration_tests]
target deploy = [app, migrations]
```

## 5. Providers

A **provider** implements the `Provider` trait and groups related resources and shared functions. A **resource** implements the `Resource` trait and handles a single block type. For example, the `go` provider contains `go.binary` and `go.test` resources. Providers are the sole extensibility mechanism — bit has no built-in knowledge of any language, tool, or platform.

### 5.1 Resource Kinds

- **`build`** (default) — cached, skipped if inputs unchanged.
- **`test`** — cached like build blocks, results aggregated, failure stops downstream blocks.

### 5.2 Provider Functions

Pure functions exported by the provider. Available in expressions under the provider namespace:

```
url = net.util.join_host_port(db.host, db.port)
```

Utility-only providers (no resources, only functions) are valid.

### 5.3 Resolve

Resources discover the full input set from minimal user configuration. This is what allows `.bit` files to be terse — the user writes `main = "./cmd/server"` and the resource discovers every contributing source file.

Resolve returns:

- **inputs** — concrete files with content hashes, forming the cache key.
- **watches** — glob patterns monitored by the runtime. Any change to matching files (added, removed, or modified) invalidates the resolution.
- **platform** — list of platform strings relevant to caching (see section 8).

```
resolve-result {
  inputs: [
    { key: "source_files", paths: [
      { path: "cmd/server/main.go",  hash: "sha256:a1b2..." },
      { path: "pkg/api/handler.go",  hash: "sha256:c3d4..." },
    ]},
    { key: "module_files", paths: [
      { path: "go.mod", hash: "sha256:1111..." },
    ]}
  ],
  watches: [
    "cmd/server/*.go",
    "pkg/api/*.go",
    "go.mod",
    "go.sum",
  ],
  platform: ["linux", "x86_64", "glibc-2.31"],
}
```

## 6. Lifecycle

Every block goes through up to four phases: **resolve**, **plan**, **apply**, and **destroy**. The lifecycle is the same for all providers. Whether a provider manages external resources or produces local files is an implementation detail — the protocol is identical.

### 6.1 Resolve

Resolve discovers the full set of inputs from the user-supplied configuration. The provider inspects source files, parses dependency graphs, and returns concrete files with content hashes, plus glob patterns for the runtime to watch.

For a `go.binary` block where the user writes `main = "./cmd/server"`, resolve follows the Go import graph and returns every `.go` file that contributes to the binary, plus watch patterns like `cmd/server/*.go` and `pkg/api/*.go`. A `kubernetes.deployment` provider may have no files to discover — resolve can return an empty input set, and the cache key is computed from user-supplied fields alone.

**When resolve runs:**
- On first build: always.
- On subsequent builds: only if watch patterns show a change (files added, removed, or modified). If watches are clean, the cached resolution is reused.

### 6.2 Plan

The runtime first checks the cache. If the cache key matches a previous result, the block is skipped — this applies to all providers uniformly.

On a cache miss, the runtime calls the provider's `plan` function with the current inputs and prior state (if any). Plan returns one of: `create`, `update`, `replace`, `destroy`, or `none`.

A `go.binary` provider receiving no prior state returns `create`. A `kubernetes.deployment` provider compares inputs to prior state and returns `update` if the replica count changed, `replace` if the namespace changed. The protocol is the same — providers make finer-grained decisions when they have state to compare against.

When a block plans `create` and has no prior outputs, downstream blocks that reference its outputs are shown as "depends on (to be created)" rather than being planned in detail. Full plan-time propagation of unknown values may be added in a future version.

**Protected blocks** — plan refuses `replace` or `destroy`. If the inputs would require either, the runtime errors and stops.

### 6.3 Apply

Apply executes the planned action and returns an `apply-result` containing `outputs` (fields that downstream blocks can reference) and `state` (an opaque blob, or nil on failure). On failure it returns an error.

The runtime persists non-nil state in the configured backend. On the next run, this state is passed back to `plan` and `apply` as `prior_state`. If state is nil, nothing is persisted.

**Test blocks** (`kind = "test"`) — apply runs the tests and returns normal outputs including `passed` (bool) and `report` (path to JUnit XML). If `passed` is false, the runtime stops downstream blocks. This is distinct from an error return, which indicates the test execution itself failed (e.g. the test binary crashed).

### 6.4 Destroy

Destroy removes everything a block has produced. The runtime calls the provider's `destroy` function with the prior state. The provider cleans up — deleting cached outputs for a build provider, tearing down external resources for an infrastructure provider. The runtime then removes any persisted state.

`bit destroy` calls destroy on all blocks in reverse dependency order — dependents are destroyed before their dependencies.

**Protected blocks** — destroy is a no-op. The runtime logs a notice and skips.

### 6.5 Refresh

Refresh queries the actual state of a resource and updates the persisted state to match reality. This detects drift — e.g. a deployment was manually scaled, a database was modified outside of bit.

The runtime calls `refresh` with the prior state. The provider queries the real resource and returns updated outputs and state. Providers with no external resources to query can return immediately.

### 6.6 Rollback

**TBD.** If apply succeeds for a block but a downstream block fails (e.g. canary deploys but smoke tests fail), there is currently no mechanism to automatically roll back the already-applied blocks. This is a known gap. Future versions may support rollback policies or compensating actions.

### 6.7 Phase Summary

|  | Resolve | Plan | Apply | Destroy |
|--|---------|------|-------|---------|
| **All providers** | Discovers inputs, returns watches | Cache check → call plan on miss | Returns outputs + optional state | Cleans up outputs and/or external resources |
| **Protected** | Normal | Refuses replace/destroy | Normal for create/update | No-op |
| **Test** | Normal | Normal | Returns passed + report | Normal |

## 7. The `exec` Provider

`exec` is a built-in provider implemented by the runtime. It follows the same lifecycle as any other provider — resolve, plan, apply — but requires no separate implementation. It is a convenience for wrapping CLI tools.

Three fields: `command`, `inputs`, and `output`.

```
docs = exec {
  command = "mdbook build docs/ -d ${output}"
  inputs  = ["docs/**/*.md", "book.toml"]
  output  = "book/"
}
```

`inputs` is a list of globs. They serve as both the cache inputs (expanded to concrete files and hashed) and the watch patterns (monitored for changes). `command` is the shell command to run. `output` is the output directory or file.

Dynamic input discovery uses `exec()` in the expression language:

```
server = exec {
  command = "go build -o ${output}/server ./cmd/server"
  inputs  = ["go.mod", "go.sum"]
            + exec("go list -deps -f '{{.Dir}}/*.go' ./cmd/server/...") | lines
  output  = "server"
}
```

The `exec()` call in `inputs` runs during input evaluation, before the block's `command` runs. Its output is split into lines and merged with the static globs. All entries are treated as globs — expanded to concrete files for hashing, watched for changes.

For test blocks, set `kind = "test"` and produce JUnit XML:

```
linting = exec {
  kind    = "test"
  command = "golangci-lint run --out-format junit-xml > ${output}/report.xml"
  inputs  = ["**/*.go", "go.mod", "go.sum"]
  output  = "lint-results/"
}
```

If an `exec` block becomes complex, that's the signal to write a real provider.

## 8. Caching and Invalidation

### Cache Key

```
cache_key = hash(provider_version, resource, user_inputs, resolved_input_hashes, platform)
```

### Platform

The runtime collects a platform fingerprint (OS, architecture, OS version). Providers declare which platform components are relevant to their cache key via a `platform` field in the resolve result — a list of strings the provider considers significant. A `c.object` provider might return `["linux", "x86_64", "glibc-2.31"]`. A `postgres.database` provider returns `[]` because it doesn't produce platform-dependent outputs.

This is provider-driven because only the provider knows what matters. A Go cross-compilation with `GOOS=linux GOARCH=amd64` produces the same output regardless of the host OS, so the Go provider would include the *target* platform, not the host. A C provider using the system compiler cares about the host libc version. A Docker image build might care about nothing if building for a fixed platform.

### Invalidation

1. **Check watches.** Compare current file set + hashes against cached snapshot. If identical, resolution is valid.
2. **Resolution valid → check cache key.** Hit → skip. Miss → apply.
3. **Resolution stale → re-resolve.** Get new inputs/watches, recompute cache key, check cache, apply on miss.

All blocks are cached identically. Same inputs and same provider produce the same cache key. If the cache key matches, the block is skipped.

## 9. Tests

Tests are blocks whose resource has `kind = "test"`. No `test` keyword in the language.

Providers output test results as **JUnit XML** — the de facto standard understood by every CI system, IDE, and reporting tool. Providers convert from their native format (go test JSON, TAP, etc.) to JUnit XML. The runtime parses it for summary display and failure propagation.

Test blocks expose two outputs: `report` (path to the JUnit XML file) and `passed` (bool). `passed = false` stops all downstream blocks.

## 10. State

When a provider returns non-nil `state` in its `apply-result`, the runtime persists it in the configured backend. State is serialized as JSON (`serde_json::Value`). On subsequent runs, this state is passed back as `prior_state`. Providers that return nil state have nothing persisted — the runtime treats them identically, there is no separate category of "stateful" vs "stateless" provider.

### Protected Blocks

Blocks can be prefixed with the `protected` modifier. Destroy becomes a no-op for protected blocks — `bit destroy` skips them and logs a notice. Plan will also refuse actions that would replace or destroy a protected block.

```
protected prod_db = aws.aurora {
  cluster = "myapp-prod"
  engine  = "aurora-postgresql"
  version = "16.1"
}
```

## 11. Rust Traits

```rust
pub type Map = HashMap<String, Value>;

pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Map(Map),
    Null,
}

pub struct ResolveResult {
    pub inputs: Vec<ResolvedInput>,
    pub watches: Vec<String>,
    pub platform: Vec<String>,
}

pub struct ResolvedInput {
    pub key: String,
    pub paths: Vec<ResolvedPath>,
}

pub struct ResolvedPath {
    pub path: String,
    pub content_hash: String,
}

pub enum PlanAction {
    Create,
    Update,
    Replace,
    Destroy,
    None,
}

pub struct PlanResult {
    pub action: PlanAction,
    pub description: String,
}

pub enum Type {
    String,
    Int,
    Bool,
    List(Box<Type>),
    Map,
    Path,
    Secret,
}

pub struct FuncParam {
    pub name: String,
    pub typ: Type,
}

pub struct FuncSignature {
    pub name: String,
    pub params: Vec<FuncParam>,
    pub returns: Type,
}

pub enum ResourceKind {
    Build,
    Test,
}

pub struct ApplyResult {
    pub outputs: Map,
    pub state: Option<serde_json::Value>,
}

/// A provider groups related resources and shared functions.
/// eg. "go" contains go.binary, go.test, etc.
pub trait Provider {
    fn name(&self) -> &str;
    fn resources(&self) -> Vec<Box<dyn Resource>>;
    fn functions(&self) -> Vec<FuncSignature>;
    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value>;
}

/// A resource implements a single block type.
/// eg. "binary" within the "go" provider.
pub trait Resource {
    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;

    fn resolve(&self, inputs: &Map) -> Result<ResolveResult>;
    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult>;
    fn apply(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<ApplyResult>;
    fn destroy(&self, prior_state: &serde_json::Value) -> Result<()>;
    fn refresh(&self, prior_state: &serde_json::Value) -> Result<ApplyResult>;
}
```

External providers will be supported via WASM in the future.

## 12. Execution Model

```
parse .bit files
  ▼
build DAG from references
  ▼
for each block (topological order, parallel where possible):
  ├─ check watches → unchanged → check cache → hit → skip
  └─ changed or miss:
      ├─ resolve
      ├─ plan
      └─ apply → store outputs + state
```

## 13. CLI

Commands map to lifecycle phases:

| Command | Phases | Description |
|---------|--------|-------------|
| `bit list` | — | List top-level targets |
| `bit plan [target]` | resolve → plan | Show what would change without applying |
| `bit apply [target]` | resolve → plan → apply | Apply all blocks |
| `bit test [target]` | resolve → plan → apply | Apply test blocks and their dependencies |
| `bit destroy [target]` | destroy | Destroy blocks in reverse dependency order |
| `bit refresh [target]` | refresh | Query real state of blocks, update stored state |
| `bit watch [target]` | resolve → plan → apply (on change) | Continuous rebuild on file changes |
| `bit status` | — | Show current state of all blocks |
| `bit clean` | — | Clear local cache |

### Watch Mode

`bit watch` uses the watch patterns already returned by providers during resolve. The runtime subscribes to filesystem notifications for all watch globs across the target's dependency graph. When a change is detected, it re-runs the standard invalidation and rebuild cycle for affected blocks and their downstream dependents.

Because watches are provider-defined, watch mode is precise without any user configuration. The Go provider watches `*.go` files in the relevant packages. The C provider watches source files and their included headers. A Kubernetes provider watches manifest files. Each provider already knows what matters.

```
$ bit watch deploy

  watching 23 patterns across 8 blocks...

  [12:01:03] src/api/handler.go changed
  [12:01:03] ✓ server           0.8s (rebuilt)
  [12:01:04] ✓ image            3.2s (rebuilt)
  [12:01:07] ✓ staging_canary   4.1s (updated)

  [12:03:44] migrations/005.sql added
  [12:03:44] ✓ staging_migrations  1.1s (applied)
```

## 14. Configuration

```toml
# bit.toml
root = "main.bit"

[state]
backend = "local"
path    = ".bit/state"

[cache]
dir = ".bit/cache"

[providers]
path = [".bit/providers", "~/.bit/providers"]
```

## 15. Example: Source to Production

```
# ── tests/smoke.bit ──

param base_url : string

health = http.test {
  base_url = base_url
  cases = [
    { method = "GET", path = "/health", expect_status = 200 },
    { method = "GET", path = "/ready",  expect_status = 200 },
  ]
}

e2e = playwright.test {
  source   = glob("tests/e2e/**/*.ts")
  base_url = base_url
}

target all = [health, e2e]
```

```
# ── deploy.bit — reusable rollout with canary analysis ──

param environment : string
param image       : string
param registry    : string
param db_url      : string
param replicas    : int
param depends_on  : list = []

e2e_image = docker.image {
  dockerfile = "tests/e2e/Dockerfile"
  tag        = "${registry}/myapp-e2e:latest"
  push       = true
}

migrations = sql.migrations {
  database   = db_url
  source     = glob("migrations/*.sql")
  depends_on = depends_on
}

app = argo.rollout {
  name       = "myapp"
  namespace  = "myapp-${environment}"
  image      = image
  replicas   = replicas
  depends_on = [migrations]
  env        = { DATABASE_URL = db_url }

  strategy = "canary"
  steps = [
    { weight = 10 },
    { pause = "30s" },
    { analysis = "smoke" },
    { weight = 50 },
    { analysis = "e2e" },
    { weight = 100 },
  ]

  analysis "smoke" {
    metrics = [
      {
        name     = "health"
        provider = "web"
        address  = "http://myapp-canary.myapp-${environment}/health"
        expect   = 200
      },
      {
        name      = "error-rate"
        provider  = "prometheus"
        query     = "sum(rate(http_requests_total{status=~\"5.*\",app=\"myapp\"}[1m])) / sum(rate(http_requests_total{app=\"myapp\"}[1m]))"
        threshold = 0.01
      },
    ]
  }

  analysis "e2e" {
    metrics = [
      {
        name     = "playwright"
        provider = "job"
        image    = e2e_image.ref
        env      = { BASE_URL = "http://myapp-canary.myapp-${environment}" }
        timeout  = "300s"
      },
    ]
  }
}

output endpoint = app.endpoint

target deploy = [app]
```

```
# ── main.bit ──

let git_sha  = exec("git rev-parse --short HEAD") | trim
let registry = "registry.example.com"

# ── Build ──

server = go.binary { main = "./cmd/server" }

image = docker.image {
  dockerfile = "Dockerfile"
  tag        = "${registry}/myapp:${git_sha}"
  push       = true
}

unit_tests = go.test { package = "./...", race = true }

# ── Local ──

test_db = docker.container {
  image        = "postgres:16"
  env          = { POSTGRES_DB = "test", POSTGRES_PASSWORD = "test" }
  ports        = ["5432:5432"]
  health_check = { command = "pg_isready", interval = "1s", retries = 10 }
}

test_app = docker.container {
  image = image.ref
  env   = { DATABASE_URL = "postgres://postgres:test@${test_db.host}:5432/test" }
  ports = ["8080:8080"]
}

local_tests = ./tests/smoke.bit {
  base_url = "http://${test_app.host}:8080"
}

# ── Staging (k8s operator) ──

staging_db = kubernetes.postgres {
  name      = "myapp-staging"
  namespace = "myapp-staging"
  version   = "16"
  storage   = "20Gi"
}

staging = ./deploy.bit {
  environment = "staging"
  image       = image.ref
  registry    = registry
  db_url      = staging_db.url
  replicas    = 3
  depends_on  = [unit_tests, local_tests.all]
}

# ── Production (Aurora) ──

protected prod_db = aws.aurora {
  cluster    = "myapp-prod"
  engine     = "aurora-postgresql"
  version    = "16.1"
  instances  = 3
  class      = "db.r6g.xlarge"
  region     = "us-east-1"
}

production = ./deploy.bit {
  environment = "production"
  image       = image.ref
  registry    = registry
  db_url      = prod_db.url
  replicas    = 10
  depends_on  = [staging.deploy]
}

target build   = [server, image]
target test    = [unit_tests, local_tests.all]
target staging = [staging.deploy]
target deploy  = [production.deploy]
```
