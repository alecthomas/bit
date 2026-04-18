# bit end-to-end example

A minimal three-tier app orchestrated by a single `BUILD.bit`:

| Tier     | Stack                      | Location    |
|----------|----------------------------|-------------|
| frontend | Vite + React + TypeScript  | `frontend/` |
| BFF      | Express + TypeScript       | `bff/`      |
| backend  | Go + Postgres (pgx)        | `cmd/`, `internal/` |

The BFF serves the built frontend bundle and proxies `/api/*` to the Go backend. The backend persists users in Postgres.

## Prerequisites

- `go` 1.24+
- `pnpm`
- `docker`
- `bit`

## Quick start

```sh
bit --list                  # list blocks
bit --plan                  # show what would change
bit                         # build + test everything
bit build                   # build artifacts only
bit test                    # run all tests (spins up postgres)
bit --clean                 # tear down (including the postgres container)
```

## What `BUILD.bit` does

| Block            | Provider          | Purpose |
|------------------|-------------------|---------|
| `node_modules`   | `pnpm.install`    | Install workspace deps; auto-discovers every `package.json` |
| `frontend`       | `pnpm.run`        | `pnpm --filter frontend run build` → `frontend/dist` |
| `bff`            | `pnpm.run`        | `pnpm --filter bff run build` → `bff/dist` |
| `backend`        | `go.exe`          | `go build ./cmd/server` → `dist/backend` (native, for local run) |
| `backend-linux`  | `go.exe`          | Linux cross-build → `dist/backend-linux` (for Docker image) |
| `backend-image`  | `docker.image`    | `example-backend:latest` from `dist/backend-linux` |
| `bff-image`      | `docker.image`    | `example-bff:latest` from `bff/dist` + runtime deps |
| `postgres`       | `docker.container`| `postgres:16` with a `pg_isready` healthcheck |
| `backend-test`   | `go.test`         | `go test ./...` (depends on `postgres`) |
| `bff-test`       | `pnpm.test`       | `vitest run` in `bff/` |
| `frontend-test`  | `pnpm.test`       | `vitest run` in `frontend/` |

The `-image` blocks are not in the default `build` target — run them explicitly:

```sh
bit backend-image bff-image
```

## Running the app locally

After `bit build`:

```sh
docker run --rm -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=app -p 5432:5432 postgres:16 &

./dist/backend &                        # :8080
node bff/dist/index.js &                # :3000, proxies /api -> :8080
open http://localhost:3000
```

Or use the `postgres` container bit already manages:

```sh
bit postgres                            # ensure the container is up
./dist/backend &
node bff/dist/index.js &
```

## Layout

```
example/
├── BUILD.bit
├── docker/             # Dockerfiles for backend and bff images
├── go.mod              # Go module at the root — go.exe walks up from cwd
├── cmd/server/         # Go HTTP server
├── internal/users/     # Postgres-backed user store
├── package.json        # pnpm workspace root
├── pnpm-workspace.yaml
├── bff/                # Express + TS
└── frontend/           # Vite + React + TS
```
