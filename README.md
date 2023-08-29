# Bit - A simple yet powerful build tool [![CI](https://github.com/alecthomas/bit/actions/workflows/ci.yml/badge.svg)](https://github.com/alecthomas/bit/actions/workflows/ci.yml)

![Build IT](bit.png)

Bit's goal is to be a simple yet powerful local build system. It is inspired
by [Make](https://www.gnu.org/software/make/manual/make.html), with the
following goals:

- Simple declarative file format.
- Leverage existing Unix knowledge.
- Deterministic, incremental, parallel builds.
- As "type safe" as possible while maintaining usability.
- Ability to build virtual targets (e.g. Docker images, Kubernetes resources, etc.).
- Great error messages.

Non-goals:

- No need to learn a new Turing complete build language and
  associated libraries (e.g. Bazel, Gradle, etc.).

Bit is driven by a configuration file called a `Bitfile`. It is described below.

## Motivation

While I love the simplicity of `make`, it has some pretty big limitations:

- If a target fails to build an output, it will still succeed, and the 
  target will silently continue to be out-of-date.
- Similarly, with variable interpolation, if a variable is undefined it will
  silently be interpolated as an empty string.
- Make can't (natively) capture non-filesystem dependencies. For example, if a
  target depends on a Docker image, it can't be expressed without intermediate
  files being manually created to track this.

## Bitfile

The Bitfile is a declarative file that describes how to build targets. It consists
of targets, templates, and variables, that can be substituted into targets.

### Variables

Variables are in the form:

```
var = value
```

They can be set on the command line, at the top level of a `Bitfile`,
or in a target. Variables are interpolated with the syntax `%{var}`. Interpolation 
occurs after inheritance and before any other evaluation.

Directive names are reserved words and cannot be used as variable names.

### Command substitution

Command substitution is in the form:

```
%(command)
```

### Targets

Targets are in the form:

```
output1 output2 ...: input1 input2 ...
  < other-target                            # Inherit from a target
  < template(arg1=value, arg2=value, ...)   # Inherit from a template
  var = value                               # Set variable
  -var                                      # Delete variable
  var += value                              # Append to variable
  var ^= value                              # Prepend to variable
  directive: parameters                     # Replace inherited directive
  -directive                                # Delete inherited directive
  +directive: parameters                    # Append to inherited directive
  ^directive: parameters                    # Prepend to inherited directive
```


### Virtual targets

Virtual targets do not exist in the filesystem, but instead refer to some virtual resource.
Examples might include Docker images, Kubernetes resources, an object in S3, etc.

They must be `hash`able and include the `create` directive.

Virtual targets have the syntax:

```
virtual name: [dependency1 dependency2 ...]
  hash: ...
  create: ...
  ...
```

Here's an example of a `Bitfile` that represents a Docker image and 
the running container built from it, rebuilding/restarting both when the
Dockerfile or any of the files in the current directory change:

```
virtual docker-container: docker-image
  hash: docker inspect docker-container
  create: docker run --restart=always -d docker-image
  delete: docker rm -f docker-container
  
virtual docker-image: Dockerfile ./**
  hash: docker image inspect docker-image
  create: docker build -f Dockerfile -t docker-image .
  delete: docker rmi docker-image
```

### Templates

Templates are targets that can be inherited from. They are in the form:

```
[virtual] template name(arg1, arg2, ...) [input1 input2 ...]: [dependency1 dependency2 ...]
  ...
```

Arguments are interpolated into the directives using the syntax `%{arg1}`, in order
to differentiate them from shell variable interpolation.

When calling a template, arguments are always named. Templates can be invoked
directly from the command line by providing arguments in the form
`name:arg1=value,arg2=value,...`.

In addition to being inheritable, templates can also be used as direct dependencies:

```
target: template(arg1=value, arg2=value, ...)
  ...
```

### Dependencies

Dependencies are targets that must be built before the current target. They may be either
virtual targets or files on the local filesystem. For files, globs may be used.

### Inheritance

Targets can inherit from other targets or templates with the `<` operator:

```
target: dependency
  < other-target                            # Inherit from a target
  < template(arg1=value, arg2=value, ...)   # Inherit from a template
```

When inheriting, existing directives can be replaced, deleted, appended or prepended to 
using the syntax:

```
directive: parameters                     # Replace inherited directive
-directive                                # Delete inherited directive
+directive: parameters                    # Append to inherited directive
^directive: parameters                    # Prepend to inherited directive
```

Similarly, variables can be set, deleted, appended, or prepended to using the syntax:

```
var = value                               # Set variable
-var                                      # Delete variable
var += value                              # Append to variable
var ^= value                              # Prepend to variable
```

### Implicit default target

An implicit default target exists for files and directories on the
local filesystem. For files, the content of the file is used as the hash.
For directories, the content of every file in the directory is used
(non recursively).

eg. for files

```
%{file}:
  hash: cat %{file}
```

And for directories:

```
%{dir}: %{dir}/**
```

This is primarily useful as a dependency of another target.

eg. given

```
file.o: file.c file.h
  create: gcc -c file.c -o file.o
```

The expansion is:

```
file.o: file.c file.h
  create: gcc -c file.c -o file.o
  
file.c:
  hash: cat file.c
  
file.h:
  hash: cat file.h
```

### Directives

Directives are in the form:

```
directive: parameters
```

A newline followed by an indent indicates a multi-line directive. In
this case all leading whitespace is stripped.

```
directive:
  parameter
  parameter
```

Available directives are:

#### `hash` directive (optional)

The `hash` directive runs a command, hashes its output along with any dependencies, 
and stores it in the Bit database as the current state of the target. If the command
fails the target is assumed to be out-of-date. When building, the hash is recomputed
and compared with the existing state. If it has changed the target is rebuilt and
the new hash stored.

If omitted, the output is hashed.

```
hash: command
```

#### `create` directive (optional)

The `create` directive runs a command if the target is not up-to-date.

If the `create` directive is omitted and the target is not up-to-date, the build
will fail.

```
create: command
```

#### `delete` directive (optional)

The `delete` directive runs a command to delete the target.

```
delete: command
```

It is optional and if omitted, the target is not deleted when cleaning.

### `dir` directive (optional)

The `dir` directive sets the working directory for the target. If omitted it
defaults to the current working directory.

The working directory is set before any other directives, variables, or file
globs are evaluated.

```
dir: path
```

### Example

```
dest = ./build
version = %(git describe --tags --always)

virtual k8s-postgres:
  < k8s-apply(manifest="db.yml", resource="pod/ftl-pg-cluster-1-0"):
  dir: ./db

virtual k8s-ftl-controller: k8s-postgres
  < k8s-apply(manifest="ftl-controller.yml", resource="deployment/ftl-controller")
  dir: ./ftl-controller

virtual release: %{dest}/ftl %{dest}/ftl-controller %{dest}/ftl-runner \
    docker-ftl-runner docker-ftl-controller

%{dest}/ftl:
  < go-cmd(pkg="./cmd/ftl")

%{dest}/ftl-controller:
  < go-cmd(pkg="./cmd/ftl-controller")

%{dest}/ftl-runner:
  < go-cmd(pkg="./cmd/ftl-runner")
  +build: echo "Runner built"

dist/*: src/** *.json *.ts *.js plop/**
  dir: console/client
  build: npm install && npm run build

protos/**/*.go console/client/src/protos/**/*.ts
    backend/common/3rdparty/protos/**/*.go: protos/**.proto buf.work.yaml **/buf.yaml **/buf.gen.yaml
  build:
    buf format -w
    buf lint
    (cd protos && buf generate)
    (cd backend/common/3rdparty/protos && buf generate)

db.go models.go queries.sql.go \
    %(shell grep -q copyfrom queries.sql && echo copyfrom.go):
  dir: backend/controller/internal/sql
  inputs:
    sqlc.yaml
    schema/*.sql
    queries.sql
  build:
    sqlc generate -f ../../../../sqlc.yaml --experimental
    # sqlc 1.18.0 generates a file with a missing import
    gosimports -w querier.go

virtual docker-ftl-runner:
  < docker(dockerfile="Dockerfile.runner", tag="ghcr.io/tbd54566975/ftl-runner:latest")

virtual docker-ftl-controller:
  < docker(dockerfile="Dockerfile.controller", tag="ghcr.io/tbd54566975/ftl-controller:latest")

build/libs/ftl-runtime.jar: src/** build.gradle.kts gradle.properties settings.gradle.kts
  dir: kotlin-runtime/ftl-runtime
  build: gradle jar

template go-cmd(pkg):
  inputs: %(go list -f '{{ join .Deps "\n" }}' %{pkg} | grep github.com/TBD54566975/ftl | cut -d/ -f4-)
  build: go build -tags release -ldflags "-X main.version=%{version}" -o %{output} %{pkg}

template k8s-apply(manifest, resource): %{manifest}
  hash: kubectl get -o yaml %{resource}
  build: kubectl apply -f %{manifest}
  delete: kubectl delete %{resource}

template docker(dockerfile, tag, context="."): %{dockerfile} %{context}
  hash: docker image inspect %{tag}
  build: docker build -f %{dockerfile} -t %{tag} %{context}
  delete: docker rmi %{tag}
```