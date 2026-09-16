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
- `k3d` + `kubectl` (for Kubernetes deployment, installed via Hermit)

## Quick start

```sh
bit --list                  # list blocks
bit --plan                  # show what would change
bit                         # build + test everything (default target)
bit build                   # build artifacts only
bit test                    # run all tests (spins up postgres)
bit dev                     # bring up the full stack on a Docker network
bit k8s                     # deploy to local k3d cluster
bit --clean ...             # tear down every block (including containers)
```

Open <http://localhost:3000> after `bit dev`.

## What `BUILD.bit` does

| Block            | Provider          | Purpose |
|------------------|-------------------|---------|
| `node_modules`   | `pnpm.install`    | Install workspace deps; auto-discovers every `package.json` |
| `frontend`       | `pnpm.run`        | `pnpm --filter frontend run build` → `frontend/dist` |
| `bff`            | `pnpm.run`        | `pnpm --filter bff run build` → `bff/dist` |
| `backend`        | `go.exe`          | `go build ./cmd/server` → `dist/backend` (native, for local run) |
| `backend-linux`  | `go.exe`          | Linux cross-build → `dist/backend-linux` (for Docker image) |
| `backend-image`  | `docker.image`    | `example-backend:latest` from `dist/backend-linux` |
| `bff-image`      | `docker.image`    | `example-bff:latest` bundling `bff/dist` + `frontend/dist` |
| `network`        | `docker.network`  | `example-net` — shared bridge for the runtime containers |
| `postgres`       | `docker.container`| `postgres:16` on the network, `pg_isready` healthcheck |
| `backend-container` | `docker.container` | Runs `example-backend:latest`, healthcheck on `/health` |
| `bff-container`  | `docker.container`| Runs `example-bff:latest`, healthcheck on `/healthz` |
| `k3d`            | `exec`            | Create local k3d cluster from config |
| `k3d-import`     | `exec`            | Import `example-backend` + `example-bff` images into k3d |
| `k8s-deploy`     | `exec`            | `kubectl apply -k` the local kustomize overlay |
| `backend-test`   | `go.test`         | `go test ./...` (depends on `postgres`) |
| `bff-test`       | `pnpm.test`       | `vitest run` in `bff/` |
| `frontend-test`  | `pnpm.test`       | `vitest run` in `frontend/` |

The image, network, and runtime-container blocks are not in the default `build`/`test` targets. Trigger them explicitly:

```sh
bit backend-image bff-image   # build container images
bit dev                       # build everything and bring up the stack
bit k8s                       # deploy to local k3d cluster
```

## Running the app

`bit dev` builds the images and brings up the full stack on `example-net`:

```
bff-container :3000  ──────▶  backend-container :8080  ──────▶  postgres :5432
      │                                                            (host :5432)
      └── also serves the React bundle at /
```

Then:

```sh
open http://localhost:3000
curl -sS http://localhost:3000/api/users
curl -sS -X POST http://localhost:3000/api/users \
  -H 'Content-Type: application/json' -d '{"name":"ada"}'
```

To stop the stack: `bit --clean ...` (the `...` tells bit to destroy every block, not just those reachable from the default target).

## Kubernetes (local k3d)

`bit k8s` builds the images, creates a local k3d cluster, imports the images, and applies kustomize manifests:

```sh
bit k8s                       # full deploy: build → k3d cluster → import images → apply
open http://localhost:3001    # BFF is port-mapped from k3d to host :3001
```

The kustomize layout separates base manifests from environment overlays:

```
k8s/
├── base/               # Generic manifests (any cluster)
│   ├── kustomization.yaml
│   ├── namespace.yaml
│   ├── postgres/       # Deployment + Service + PVC
│   ├── backend/        # Deployment + Service
│   └── bff/            # Deployment + Service
└── overlays/
    └── local/          # k3d-specific: imagePullPolicy Never, port mapping
        ├── kustomization.yaml
        └── k3d-cluster.yaml
```

To tear down: `bit --clean k8s` destroys the k3d cluster and removes stamp files.

## Layout

```
example/
├── BUILD.bit
├── docker/             # Dockerfiles for backend and bff images
├── k8s/                # Kustomize manifests (base + overlays)
├── go.mod              # Go module at the root — go.exe walks up from cwd
├── cmd/server/         # Go HTTP server
├── internal/users/     # Postgres-backed user store
├── package.json        # pnpm workspace root
├── pnpm-workspace.yaml
├── bff/                # Express + TS
└── frontend/           # Vite + React + TS
```
