[![codecov](https://codecov.io/github/OctaHive/octa/branch/main/graph/badge.svg?token=Q1UWZ4QHGZ)](https://codecov.io/github/OctaHive/octa)
![build](https://github.com/OctaHive/octa/actions/workflows/tests.yml/badge.svg)
![License: MIT](https://img.shields.io/github/license/adrianmrit/mom)

# Inspiration
This project was inspired by the go-task build project. However, when we rewriting our project’s build system to go-task, I found some 
functionality missing, so I decided to create my own builder. 

# The differences from go-task
* Support for a plugin system to extend the builder’s functionality
* Support run tasks using wildcards, for example `*:docker` run all first child docker tasks, or you can run `**:docker` and run all nested docker task
* Support true parallel execution of commands within a single task
* Support returning dependency task results. This is useful, for example, when you need to process the result of a task in its parent task.
* Support rendering templates and return result of rendering as task result

# Installation

## Homebrew
If you have homebrew installed, you can install octa with:
```console
brew tap OctaHive/octa
brew install octa
```

## Binary releases
Binaries are also available for Windows, Linux and macOS under [releases](https://github.com/OctaHive/octa/releases). To install, download the binary for 
your system and add to your `$PATH`.

# Getting started
Create a file called `Octafile.yml` in the root of your project and add your tasks to `tasks` section. In `cmds` attribute of task you need 
provide a set of commands for your task. Here the simple example of Octafile for building go service and docker image for service.

```yaml
version: 1

vars:
  PORT: 11001
  SERVICE: service-name
  VERSION: 1.0.0
  MAINTAINER: "My Cool company <support@cool-company.com>"
  PROJECT: my-project

tasks:
  build:
    cmds: 
      - go build -o service main.go
    
  docker:
    cmds: |
      docker build \
        --build-arg PORT={{ PORT }} \
        --build-arg MAINTAINER="{{ MAINTAINER }}" \
        --build-arg VERSION={{ VERSION }} \
        -t {{ PROJECT }}/{{ SERVICE }}:{{ VERSION }} \
        --pull=true \
        --network=host \
        --file Dockerfile .
```

After creating a Octafile, you can run a tasks by calling `octa` and provide the names of the task to run. Provided tasks will 
be executed sequentially.

When no task name is provided, Octa runs the task named `default`. If the Octafile does not define
that task, Octa prints its command-line help instead.

# Task files
The tasks are defined using the YAML format. So to start create your build tasks you need create task config file. 
The system currently supports configuration files in the following name variants:

- Octafile.yml
- octafile.yml
- Octafile.yaml
- octafile.yaml
- Octafile.lock.yml
- octafile.lock.yml
- Octafile.lock.yaml
- octafile.lock.yaml

The purpose of using .lock variants is to provide a committed version of the file for the project while enabling individual users to 
customize the Octafile by creating their own Octafile.yml, which would be excluded from version control using .gitignore.

When executing a task, the program begins searching for configuration files in the current working directory and proceeds upward 
toward the root directory. It follows a specific order, stopping as soon as it finds either a matching file, a octafile.{yml,yaml} 
file, or reaches the root directory with no further folders to check.

To run a task from your global Octafile located in your home directory, use the `--global` or `-g` flag. This is ideal for managing
personal tasks that aren’t tied to a specific project.

You can also run tasks from a specific file by simply passing it with the `--octafile` or `-o` flag, e.g., `octa -o project_tasks.yml build`.

Use `--dir` to start Octafile discovery from another directory. Octa searches that directory and
then its parents, just as it does for the current working directory. When combined with a relative
`--octafile` path, the path is resolved relative to `--dir`.

```console
octa --dir ./backend build
octa --dir ./backend --octafile config/Octafile.yml build
```

## Default command plugin

A task or an item in `cmds` written as a plain string is handled by the configured default plugin.
Without configuration, Octa uses the task-type key returned by the built-in `shell` plugin.

Set the default globally in the file passed with `--config`:

```yaml
plugins: []
default_plugin: tpl
```

```console
octa --config octa-config.yml render
```

An Octafile can override the global value:

```yaml
version: 1
default_plugin: tpl

tasks:
  render: Hello {{ NAME }}

  explicit-shell:
    shell: echo "still handled by shell"
```

`default_plugin` is a task-type key returned by a loaded plugin, not the plugin executable name.
Unknown keys are rejected while loading the Octafile. Included Octafiles inherit the effective
default from their parent and can override it locally. Explicit plugin mappings and annotations are
never affected by this setting.

The [`example/default-plugin`](example/default-plugin) directory demonstrates both the global
setting and a local override in an included Octafile.

## Listing and searching tasks

Use `--list-tasks` to print every available task. Internal tasks are omitted from the output.

```console
octa --list-tasks
```

Use `--search` to filter the list by a task's qualified name or description. Search is
case-insensitive and includes names from nested Octafiles, such as `backend:build`.

```console
octa --search build
```

# Including task files
If you have a large project with deep nesting structure, keeping all tasks in a single file can be inconvenient. Additionally, different 
teams may be responsible for building different parts of the project. To address this, you can split your tasks across multiple files and 
include the necessary Octafiles in the main project file. To import these files, list them in the `includes` section. You can specify them 
using either a short or an extended format.

```yaml
version: 1

includes:
  # Short version, will look for ./web/Octafile.yml
  web: ./web
  
  installer: ./InstallerTasks.yml
  
  # Extended version allows for specifying additional options to configure inclusions
  backend:
    octafile: ./backend/Octafile.yml
```

All imported tasks will be accessible through a namespace based on the key name in the imports section. So, you'd call task `web:serve` to 
run the serve task from web/Octafile.yml or task `backend:build` to run the build task from the ./backend/Octafile.yml file.

The built-in `OS` and `ARCH` variables use Go-compatible values such
as `linux`, `darwin`, `windows`, `amd64`, and `arm64`. Variables declared in the current Octafile are
also available:

```yaml
version: 1

vars:
  PROFILE: development

includes:
  platform: ./Taskfile_{{OS}}.yml
  profile: ./profiles/Octafile_{{PROFILE}}.yml
```

## Monorepo discovery

A root Octafile can automatically discover Octafiles in monorepo projects:

```yaml
version: 1

monorepo:
  roots:
    - packages/*
    - services/**
  exclude:
    - target
    - node_modules
  max_depth: 5
```

`*` matches exactly one directory level. `**` searches recursively and is bounded by
`max_depth`, which defaults to `5`. Excluded directory names are pruned before traversal;
relative glob patterns can also be used for more specific exclusions. Octa always excludes
`.git` and `.octa` from discovery.

Discovered task names use the same colon-separated namespaces as explicit includes. For
example, `packages/api/Octafile.yml` exposes its `build` task as
`packages:api:build`. Wildcards work uniformly for both forms, such as
`octa 'packages:*:build'` or `octa '**:test'`.

When Octa is invoked inside a discovered project, an unqualified name such as `build`
resolves to that project. Use `::task-name` to address a task in the monorepo root. Passing
`--octafile` explicitly keeps that file as the entry point and does not activate an ancestor's
monorepo configuration.

Octa caches only the discovered project paths, not parsed tasks. The cache is reused while the
traversed directory hierarchy is unchanged and is rebuilt when directories or discovered
Octafiles are added or removed. Use `--clean-cache` to clear it manually.

See [`example/monorepo`](example/monorepo) for a runnable layout.

## Advanced including options
If you are using the extended task file import option, you can use the following settings:

##### optional
Allows execution to continue if the imported file is not found.

```yaml
version: 1

includes:
  e2e:
    octafile: ./e2e/Octafile.yml
    optional: true

tasks:
  build:
    cmds:
      # This command will be successfully executed
      - ./build.sh
```

##### dir
By default, the working directory for the imported task file will be set to the directory from which the imported file is loaded. You can 
override this behavior by specifying a directory for the nested task file.

```yaml
version: 1

includes:
  e2e:
    octafile: ./e2e/Octafile.yml
    dir: ./build
```

##### vars
You can pass variables when importing a nested task file. The provided parameters will overwrite the variables defined in the imported file.

```yaml
version: 1

includes:
  backend:
    octafile: ./shared/Docker.yml
    vars:
      BUILD_IMAGE: ubuntu

  web:
    octafile: ./shared/Docker.yml
    vars:
      BUILD_IMAGE: debian
```

# Providing arguments to task
If you want to pass arguments to the invoked tasks, you can specify them after `--`. The passed arguments will be available to the commands 
through the `COMMAND_ARGS` variable.

```yaml
version: 1

tasks:
  web:
    shell: echo {{ COMMAND_ARGS }}
```

```console
$ octa web -- publish
```

# Providing variables from the CLI

Add `NAME=VALUE` arguments to override Octafile variables for an invocation:

```console
octa build PROFILE=production VERSION=1.2.3
```

CLI variable values are strings and apply to every selected task, including watch reruns. They
have higher priority than Octafile, task, invocation, and process-environment values. Variables may
appear between task names, and the last value assigned to the same name wins. The value may contain
additional `=` characters and may reference variables provided earlier on the command line:

```console
octa test publish CHANNEL=stable IMAGE='application:{{ CHANNEL }}=latest'
```

The explicit, repeatable `--var NAME=VALUE` option remains available when its intent should be
unambiguous. Positional assignments take precedence when both forms set the same variable. Include
path templates also use CLI overrides during Octafile loading. Arguments after `--` are passed to
the task and are never interpreted as variables.

Do not pass secrets through CLI variables: command-line arguments may be stored in shell history
or visible to other processes. Use environment variables or an external secret manager instead.

# Environment variables
The env property is used to define environment variables that will be accessible to all tasks within the file. The value of the property 
is a map of key-value pairs, where the key is the name of the environment variable, and the value is the value of the environment variable.
Environment variables can be defined at different levels – at the task file level, at the task level, and when invoking a task from another 
task. As a result, commands executed within a task will receive the expandable version of the environment variables. System environment 
variables will also be added to the resulting set of variables.

```yaml
version: 1

env:
  NAME: Bob
  
tasks:
  print-env: echo $NAME
  
  print-system-env: echo ${EXT_NAME:-"Alice"}
```

So the output this task will be:

```console
$ ./octa print-env

2024-12-17 11:23:23 [octa] Starting execution plan for command print-env
2024-12-17 11:23:23 [octa] Starting task print-env
Bob
2024-12-17 11:23:23 [octa] All tasks completed successfully
2024-12-17 11:23:23 [octa] ================== Time Summary ==================
2024-12-17 11:23:23 [octa]  "print-env": 13ms
2024-12-17 11:23:23 [octa]  Total time: 13ms
2024-12-17 11:23:23 [octa] ==================================================

$ ./octa print-system-env

2024-12-17 11:23:41 [octa] Starting execution plan for command print-system-env
2024-12-17 11:23:41 [octa] Starting task print-system-env
Alice
2024-12-17 11:23:41 [octa] All tasks completed successfully
2024-12-17 11:23:41 [octa] ================== Time Summary ==================
2024-12-17 11:23:41 [octa]  "print-system-env": 13ms
2024-12-17 11:23:41 [octa]  Total time: 13ms
2024-12-17 11:23:41 [octa] ==================================================

$ EXT_NAME=Karol ./octa print-system-env

2024-12-17 11:23:51 [octa] Starting execution plan for command print-system-env
2024-12-17 11:23:51 [octa] Starting task print-system-env
Karol
2024-12-17 11:23:51 [octa] All tasks completed successfully
2024-12-17 11:23:51 [octa] ================== Time Summary ==================
2024-12-17 11:23:51 [octa]  "print-system-env": 17ms
2024-12-17 11:23:51 [octa]  Total time: 17ms
2024-12-17 11:23:51 [octa] ==================================================
```

Octa also supports loading variables from `.env` files. By default, `.env` is searched for from
the current directory upward through its parents.

Use `-e` or `--env-file` to load one or more files from explicit paths:

```console
octa --env-file config/base.env --env-file config/development.env print-file-env
```

Relative paths are resolved from the current directory. When multiple files define the same
variable, the last file wins. Variables already present in the process environment take precedence
over all files. Supplying `--env-file` disables the automatic `.env` search, and every specified
file must exist and contain valid dotenv syntax.

Dotenv files can also be configured at the Octafile and task levels:

```yaml
version: 1

vars:
  PROFILE: development

dotenv:
  - .env.{{ PROFILE }}
  - config/base.env

env:
  LOG_LEVEL: info

tasks:
  server:
    dir: services/api
    dotenv:
      - .env.local
    env:
      LOG_LEVEL: debug
    shell: ./server
```

A value containing only a file name, including a templated file name such as
`.env.{{ PROFILE }}`, is searched for from the Octafile or task directory upward through its
parents. A value containing a directory, such as `config/base.env` or `../shared.env`, is treated
as an exact path relative to that directory and must exist. Absolute paths are also loaded
directly. A missing file name searched upward is skipped.

Files are listed from highest to lowest priority, so the first file wins when several files define
the same variable. Task dotenv values override Octafile dotenv values, while an explicit `env`
value at either level overrides dotenv values from the same level. Environment values supplied by
the invoking task have the highest priority.

Environment values also support Octa templates and shell-backed values. A dynamic variable can be
exported to the process environment, or a command can produce the environment value directly:

```yaml
version: 1

tasks:
  build:
    vars:
      VERSION:
        sh: git describe --tags --always
    env:
      BUILD_VERSION: "{{ VERSION }}"
      COMMIT:
        sh: git rev-parse --short HEAD
      RELEASE: 'release-{{ "git describe --tags --always" | shell }}'
    shell: ./build.sh
```

Use `sh:` when the command produces the complete value. The `shell` Tera filter is useful when the
output is embedded in a larger template. The existing
`{{ shell(command="git describe --tags") }}` function form remains supported.

Any registered plugin can also produce a value during interpolation. Pass the plugin's task type
as `key`; the value is validated against the schema advertised by that plugin before execution:

```yaml
env:
  FROM_FUNCTION: '{{ plugin(key="tpl", value="Hello {{ NAME }}") }}'
  FROM_FILTER: '{{ "Hello {{ NAME }}" | plugin(key="tpl") }}'
```

The `shell` function, filter, and `sh:` source select whichever plugin advertises the `shell`
capability; they do not depend on its task type or executable name. The generic `plugin` forms make
the same mechanism available to third-party task plugins. Plugin-backed interpolation is available
in runtime variable and environment values, task directory templates, and preconditions.

Shell-backed values run once for each task execution and are evaluated again when watch mode reruns
the task. Octafile-level values run from their Octafile directory, while task-level environment
values run from the effective task directory. They receive environment values and dotenv entries
available at that level. A non-zero exit code fails expansion, trailing whitespace is removed from
stdout, and dry mode prints the command without executing it.

# Variables
The vars property is used to define variables that will be available to all tasks in the file. This behaves like the env property, but the 
variables are not exported to the environment, and can be more complex than strings.

Here the example of usage vars:

```yaml
version: 1

vars:
  STR: "Hello World"
  NUM: 1
  FLOAT: 1.35
  GREETING: Hello
  MESSAGE: "{{ GREETING }} World"
  OBJ:
    val: 1
  ARR: ["A", "B", "C"]
  
tasks:
  say_hi:
    shell: echo {{ STR }}
    
  plus_one:
    shell: echo {{ NUM + 1 }}
    
  print_obj:
    shell: echo {{ OBJ.val }}

  print_arr:
    shell: echo {{ ARR[0] }}
```

You can use different data types as values. The following data types are supported:
* string
* bool
* number
* float
* array
* object

Variables in the same `vars` mapping are expanded in declaration order, so a value can reference
values declared before it. Forward references are rejected because the referenced value has not
been resolved yet.

Mark a literal or shell-backed variable as secret to replace its resolved value with `*****` in
Octa diagnostics and logs produced by plugins built with the current plugin SDK:

```yaml
vars:
  API_TOKEN:
    value: "development-token"
    secret: true
  VAULT_TOKEN:
    sh: vault read -field=token secret/application
    secret: true
```

Secret values remain available to templates and commands. Output produced by the command itself is
not redacted, and a variable derived from a secret must be marked as secret separately.

Mark a variable as required when its value must be supplied by another variable layer:

```yaml
tasks:
  deploy:
    vars:
      API_TOKEN:
        required: true
        secret: true
    shell: ./deploy.sh
```

```console
octa deploy API_TOKEN=token
```

Use `required: prompt` to request a missing value interactively. `question` customizes the prompt;
when it is omitted, Octa generates a question from the variable name. An optional `enum` presents
the allowed string values as a selection:

```yaml
tasks:
  deploy:
    vars:
      ENVIRONMENT:
        required: prompt
        question: Select deployment environment
        enum:
          - development
          - staging
          - production
    shell: ./deploy.sh
```

Values supplied without a prompt are also checked against `enum`. An enum always uses the selection
UI because its options are already present in the Octafile. A secret variable without an enum uses
hidden input. `required: true` remains strictly non-interactive. A prompt also fails instead of
waiting for input when no terminal is attached or when `--non-interactive` is used, so CI runs cannot
hang.

The choices can also come from another variable. The referenced value must be a concrete list of
strings; secret and shell-backed variables are intentionally unavailable because enum choices are
shown in the terminal. Templates inside a literal or referenced list are expanded before the prompt:

```yaml
vars:
  DEPLOYMENT_ENVIRONMENTS:
    - development
    - staging
    - production

tasks:
  deploy:
    vars:
      ENVIRONMENT:
        required: prompt
        enum: "{{ DEPLOYMENT_ENVIRONMENTS }}"
    shell: ./deploy.sh
```

Prompts are resolved while Octa builds the selected execution graph, before any command starts.
Consequently, a reachable task may request its variables even when a later `if` condition skips it.

A required variable cannot define `value` or `sh`. CLI variables, process environment variables,
include variables, and variables passed by another task can satisfy the requirement. Validation is
performed after all variable layers have been merged while Octa builds the selected execution
graph; listing tasks does not trigger it. Missing, null, whitespace-only, and empty collection values
fail with an error. Supplied values must be concrete rather than `sh` or template expressions,
which makes them available while configured variables are expanded. `secret: true` also redacts a
value supplied by another layer.

When evaluating variables for a task, Octa applies them in the following order, from highest to
lowest priority:

* Values passed as named arguments when invoking Octa
* Process environment variables
* Values passed when running the task from another task
* Values defined for the task
* Values in the Octafile where the task is defined
* Values in parent Octafiles, starting with the nearest parent

```yaml
version: 1

vars:
  GREETING: Hello from Taskfile!

tasks:
  print-var:
    cmds:
      - echo "{{ VAR }}"
    vars:
      VAR: Hello!
      
  greet:
    cmds:
      - echo "{{ GREETING }}"
```

The option to pass a parameter when invoking octa:

```console
$ GREETING="Hello Bob" ./octa greet
2024-12-22 18:36:59 [octa] Building DAG for command greet with provided args []
2024-12-22 18:36:59 [octa] Starting execution plan for command greet
2024-12-22 18:36:59 [octa] Starting task echo "{{GREETING}}"
Hello Bob
2024-12-22 18:36:59 [octa] All tasks completed successfully
```

In addition to setting variables through expanding values, you can set variables using shell command 
execution.

```yaml
version: 1

tasks:
  build:
    cmds:
      - go build -ldflags="-X main.Version={{VERSION}}" main.go      
    vars:
      VERSION:
        sh: git describe --tags --abbrev=0
```

# Dry mode
Sometimes you may want to check how your task works without executing any commands. For 
this purpose, you can run the task in dry mode using the `--dry` flag. In dry mode, Octa 
will only print the commands that would be run, without actually executing them.

# Tasks
Here’s a revised version of the text:

The tasks property in the Octafile is used to define the tasks within the file. Its value 
is a map of key-value pairs, where the key represents the task name, and the value is either
the task definition or a task command for simple mode usage.

```yaml
version: 1

task:
  simple: echo Simple Task
  
  complex:
    shell: echo Complex task
```

## Task command
Each task can have commands that will be executed by the built-in shell plugin. It uses
[Brush](https://github.com/reubeno/brush), so the same Bash/POSIX-compatible syntax works on
Windows, Linux, and macOS without requiring Bash to be installed. There are two ways to set
commands in a task: `shell` and `cmds`. The `cmds` variant allows you to set multiple commands,
which will be executed in sequence.

```yaml
version: 1
  
tasks:
  simple:
    shell: echo Hello World!
    
  multiple:
    cmds:
      - echo Hello Alice!
      - echo Hello Bob!

  bash-syntax:
    shell: |
      names=(Alice Bob)
      for name in "${names[@]}"; do
        echo "Hello ${name}!"
      done
    
```

Commands run in isolated Brush child processes, preserving cancellation and task timeout behavior.
Bash scripts can be sourced directly, including on Windows:

```yaml
tasks:
  build:
    shell: source ./scripts/build.sh
```

## Plugin task annotations

A plugin schema key can be used as a YAML annotation on a task. The annotation selects the plugin,
and the tagged value is passed to that plugin as its command or parameters.

```yaml
version: 1

tasks:
  build: !shell cargo build
  render: !tpl "Hello, {{ NAME }}!"
```

The annotation name must match the key returned by a loaded plugin. Unknown annotations are
reported as Octafile parsing errors. Plugins can also return a JSON Schema for their task value;
Octa applies it to annotations, mapping-style tasks, and plugin commands in `cmds` before execution.
The existing mapping syntax remains supported.

```yaml
tasks:
  build:
    shell: cargo build
```

## Task template
Sometimes you need to simply template text and return the result to a task that depends on the 
current one. To do this, you can specify a `tpl` for the task, and when the task is executed, 
the result will be templated using the specified variables and returned as the result of the task. 
This allows you to generate configurations, such as generating a docker-compose file for your project.

The template can be defined inline or loaded from a UTF-8 file. Relative file paths are resolved
from the task working directory.

```yaml
version: 1

tasks:
  docker-compose-service:
    vars:
      SERVICE: service
      PROJECT: octa
      DOCKER_REPO: docker.octa.com
      VERSION: 1.0.0
    tpl: >-
      {{ PROJECT }}-{{ SERVICE }}:
        image: {{ DOCKER_REPO }}/{{ SERVICE }}:{{ VERSION }}
        restart: "always"
        network_mode: "host"
        logging:
          driver: json-file
          options:
            max-size: "10m"
            max-file: "10"
        environment:
          - LOG_LEVEL=Debug

  docker-compose:
    shell: echo "{{ deps_result['docker-compose-service'] }}"
    deps:
      - docker-compose-service
```

To load the same template from disk, use the object form:

```yaml
tasks:
  docker-compose-service:
    vars:
      SERVICE: service
      PROJECT: octa
      VERSION: 1.0.0
    tpl:
      file: templates/service.yml.tpl
```

If we run task `docker-compose` we see next output:

```console
2024-12-17 09:21:54 [octa] Starting execution plan for command docker-compose
2024-12-17 09:21:54 [octa] Starting task docker-compose-service
2024-12-17 09:21:54 [octa] Starting task docker-compose
octa-service:
  image: docker.octa.com/service:1.0.0
  restart: always
  network_mode: host
  logging:
    driver: json-file
    options:
      max-size: 10m
      max-file: 10
  environment:
    - LOG_LEVEL=Debug
2024-12-17 09:21:54 [octa] All tasks completed successfully
2024-12-17 09:21:54 [octa] ================== Time Summary ==================
2024-12-17 09:21:54 [octa]  "docker-compose-service": 0ms
2024-12-17 09:21:54 [octa]  "docker-compose": 10ms
2024-12-17 09:21:54 [octa]  Total time: 10ms
2024-12-17 09:21:54 [octa] ==================================================
```

## Internal task
By default, all tasks defined in the file are available for execution via the command-line utility. 
Sometimes, it may be convenient to create a task that is only available internally, for example, if 
you need to call the same command with slight parameter variations. To achieve this, you can set the 
`internal` attribute for the task, making it unavailable for execution from the CLI utility and preventing 
it from appearing in the list of available tasks when using the `--list-tasks` command.

## Task directory
By default, tasks are executed in the directory where the Octafile is located. However, you can easily 
change the working directory for a task by specifying the `dir` parameter. If that directory does not
exist, Octa creates it together with any missing parent directories before executing the task. A dry run
resolves the path without creating it.

```yaml
version: 1

tasks:
  build:
    shell: go build main.go
    dir: ./service
```

The directory attribute supports expansion, allowing you to use environment variables or variable values 
within this property. For example, since Octa supports configuration traversal, you can create a main 
Octafile in a parent directory and run a task from a service subdirectory by using the `USER_WORKING_DIR` 
variable to set the working directory to the service directory.

```yaml
version: 1

tasks:
  build:
    shell: go build main.go
    dir: "{{ USER_WORKING_DIR }}"
```

# Calling another task
In task commands, you can specify both shell commands and invoke other tasks. если 
All commands listed within a task are executed sequentially by default, but you can change this behavior using the `execute_mode` parameter. 
This parameter supports two values: `parallel` and `sequentially`, with `sequentially` being the default. If you want to execute another task, 
you can specify it by adding the `task:` prefix or use the extended version, which allows you to provide additional parameters:

##### vars
Overrides variables for the invoked task.

##### envs
Overrides environment variables for the invoked task.

##### silent
Disables output of the task’s commands to the standard output.

```yaml
version: 1

tasks:
  prev:
    shell: echo Start
  
  next:
    shell: echo Finish
    
  run:
    cmds:
      - task: prev
      - echo Running task
      - task: next
```

# Concurrency limit

Set a shared concurrency limit in the root Octafile:

```yaml
version: 1
concurrency: 4

tasks:
  build:
    execute_mode: parallel
    cmds:
      - task: frontend
      - task: backend
```

Use `--concurrency N` to override the file default for one invocation:

```bash
octa --parallel --concurrency 4 lint test build
```

The limit is shared by all selected commands and also applies to parallel task commands,
dependencies, watch reruns, and deferred commands. Internal graph nodes do not consume a slot.
Without either setting, Octa does not impose an additional concurrency limit. Both values must be
greater than zero.

# Command timeouts

Use `timeout` to stop commands that run longer than expected. A task-level timeout is the default
for every command in that task; an individual command or task reference can override it. Durations
support units such as `ms`, `s`, `m`, and `h`.

```yaml
version: 1

tasks:
  test:
    timeout: 10m
    cmds:
      - cargo test --workspace
      - shell: cargo test --test e2e_test
        timeout: 2m
      - task: integration
        timeout: 5m

  integration:
    shell: ./scripts/integration-test.sh

  deploy:
    deps:
      - task: test
        timeout: 5m
    shell: ./scripts/deploy.sh
```

Timeouts also apply to deferred and plugin commands. When a timeout expires, Octa cancels that
specific command, reports an error, and keeps the plugin available for subsequent executions.

# Fail-fast execution

By default, when one parallel task fails, Octa stops scheduling its dependants but lets work that
is already running finish. Enable `failfast` to cancel that running work immediately. It can be set
for the whole Octafile and overridden by an individual task:

```yaml
version: 1
failfast: true

tasks:
  build:
    execute_mode: parallel
    cmds:
      - echo build frontend
      - echo build backend

  integration:
    failfast: false
    execute_mode: parallel
    cmds:
      - echo test API
      - echo test database
```

The `--failfast` (`-F`) CLI option applies the behavior to all commands in the current invocation,
including commands selected together with `--parallel`:

```bash
octa --parallel --failfast lint test build
```

# Deferred commands

Use `defer` to schedule cleanup after the current task finishes. Deferred commands run after successful
execution, after a failure, and during cancellation. Multiple deferred commands run in reverse declaration
order. A deferred command is registered only after execution reaches its position in `cmds`.

```yaml
version: 1

tasks:
  build:
    cmds:
      - shell: mkdir -p build/tmp
      - defer: rm -rf build/tmp
      - defer:
          task: report
      - shell: ./build.sh

  report:
    shell: echo "Build finished"
```

`defer` supports shell commands, task references, and every plugin command. Command metadata such as
`platforms` can be placed next to `defer`:

```yaml
tasks:
  cleanup:
    cmds:
      - defer:
          shell: ./cleanup.sh
        platforms: [linux, darwin]
      - shell: ./run.sh
```

Deferred command failures are logged but do not replace the result of the main task. If the main task fails,
it remains failed after cleanup finishes.

# Platform specific tasks and commands

If you want to restrict tasks or individual commands to particular operating systems and architectures,
use the `platforms` parameter. Tasks and commands that do not match the current platform are skipped.
The current operating system is available through the `OCTA_OS` variable, and the current architecture is
available through `OCTA_ARCH`. You can specify multiple operating system and/or architecture selectors.

Each value can select an operating system (`linux`, `windows`, `macos`), an architecture (`x86_64`, `arm64`),
or an exact pair such as `linux/x86_64`. The aliases `amd64` and `x64` match `x86_64`; `aarch64` matches
`arm64`; `darwin` and `osx` match `macos`. Matching is case-insensitive and ignores whitespace.

```yaml
version: 1

tasks:
  build_win: 
    platforms: ["windows"]
    shell: echo Windows build
  
  build_mac: 
    platforms: ["macos/arm64"]
    shell: echo Mac OS build

  build_linux:
    platforms: ["linux/amd64"]
    shell: echo Linux x86-64 build
    
  build:
    cmds:
      - task: build_win
      - task: build_mac
      - task: build_linux
```

The same restriction can be applied to plugin commands and task references inside `cmds`. Commands without
`platforms` continue to run on every platform:

```yaml
version: 1

tasks:
  build:
    cmds:
      - shell: build.cmd
        platforms: [windows]
      - shell: ./build.sh
        platforms: [linux, darwin]
      - tpl: templates/config.tpl
        platforms: [linux/arm64]
      - task: package
        platforms: [linux, darwin]
      - echo "Runs on every platform"
```

# Task dependencies
Some tasks may depend on other tasks. You can list all task dependencies in the `deps` parameter, and when the task is executed, 
all its dependencies will run first, followed by the task itself. All dependent tasks are executed in parallel, so they should 
not depend on each other. You can also create grouping tasks that only contain dependencies and do not have their own commands.

Dependencies can be specified in two modes: you can reference another task by simply adding the name of task, or use the extended
version, which allows you to specify additional parameters:

##### vars
Overrides variables for the depended task.

##### silent
Disables output of the task’s commands to the standard output.

```yaml
version: 1

tasks:
  prepare_one: echo Prepare one
  prepare_two: echo Prepare two
  
  complex_task:
    shell: echo All deps task completed
    deps:
      - prepare_one
      - prepare_two
```

The output of all dependent tasks is saved and made available to the parent task. This is useful when you need to execute a 
series of subtasks and then combine their results into a single output, for example, generating a Docker Compose file for your 
product. Alternatively, you can convert the result into the desired data type and process it as needed.

```yaml
version: 1

tasks:
  task1: echo 1

  task2: echo 2

  global:
    shell: echo {{ deps_result.task1 | int + deps_result.task2 | int }}
    deps:
      - task1
      - task2
```

# Task run mode
Some of your tasks may depend on the same tasks. By default, Octa will rerun the dependent task each time, which will result in 
the dependent task being executed multiple times. You can change this behavior by setting the `run` attribute of task. The following
values are supported:

##### always
The task will be executed every time, regardless of whether it has been run before. This is the default value.

##### once
The task will be executed only once.

##### changed
The task will be executed only if the task parameters passed in the `vars` variable have changed.

```yaml
version: 1

tasks:
  long: 
    shell: sleep 10
    run: once
  
  task:
    run: changed
    shell: echo {{ CONTENT }}
    deps:
      - long
      
  test:
    deps:
      - task: task
        vars:
          CONTENT: 1
      - task: task
        vars:
          CONTENT: 2
      - task: task
        vars:
          CONTENT: 2
```

You can also set `run` at the Octafile level. It becomes the default for tasks declared in that file;
a task-level value takes precedence. Included Octafiles use their own `run` setting.

```yaml
version: 1
run: changed

tasks:
  build:
    shell: cargo build

  publish:
    run: always
    shell: cargo publish
```

# Prevent run task
Often, if your source files have not changed, there is no need to run the task. To handle this, you can specify 
the `sources` parameter for the task, where you can list the files whose changes need to be tracked. When the 
task is executed, Octa will check the checksums of the specified files, and if they have not changed, the task
will complete without being executed.

```yaml
version: 1

tasks:
  build:
    sources:
      - ./src/*
    shell: echo Run build
```

If we run this task again, the command will complete without actually executing:

```console
$ ./octa build
2024-12-22 16:59:06 [octa] Building DAG for command build with provided args []
2024-12-22 16:59:06 [octa] Starting execution plan for command build
2024-12-22 16:59:06 [octa] Starting task build
Run build
2024-12-22 16:59:06 [octa] All tasks completed successfully

$ ./octa build
2024-12-22 16:59:08 [octa] Building DAG for command build with provided args []
2024-12-22 16:59:08 [octa] Starting execution plan for command build
2024-12-22 16:59:08 [octa] Task build are up to date
2024-12-22 16:59:08 [octa] All tasks completed successfully
```

Use `output` to declare files or directories produced by the task. A missing output makes all
commands owned by that task stale. Referenced tasks keep their own independent freshness checks:

```yaml
version: 1
source_strategy: hash

tasks:
  build:
    sources:
      - ./src/**/*.rs
      - "!./src/generated/**"
      - ./Cargo.toml
    output:
      - ./target/release/*
      - "!./target/release/*.map"
    shell: cargo build --release
```

Set `source_strategy` at the Octafile level to provide the default for tasks declared in that file,
or override it for one task:

```yaml
source_strategy: hash

tasks:
  build:
    source_strategy: timestamp
    sources: [./src/**/*]
    output: [./dist/app]
    shell: ./build.sh
```

With the default `hash` source strategy, Octa compares the source fingerprint and checks that every
`output` pattern has at least one match. Output contents are not included in the hash. With
`source_strategy: timestamp`, Octa also
reruns the task when the newest source is newer than the oldest output. Output directories are
inspected recursively, while parent directory timestamps are ignored when tracked descendants
exist. Fingerprints are stored only after the main task body completes successfully, so failed and
partially completed tasks are retried. Source and output paths are resolved from the root Octafile
directory.

Freshness identities include task configuration and resolved variables explicitly declared in
Octafile or on the command line. Dynamic `sh` values are resolved once during the freshness check
and the same values are reused by every command in that task invocation. Process environment
variables remain available at runtime, but declare values that affect generated outputs in `vars`
or `env` so they are tracked without making every unrelated environment change invalidate the task.

Prefix a pattern with `!` to exclude its matches from `sources` or `output`. Patterns are applied
in declaration order, so a later positive pattern can re-include a path. Quote exclusions in YAML
to prevent `!` from being parsed as a tag. Use `\!` at the beginning for a literal path whose name
starts with `!`.

You can use glob patterns when specify source targets.

To exclude files or directories matched by `sources`, create `.octaignore` files anywhere under the root
`Octafile` directory. Each file uses `.gitignore` syntax, applies to its directory and descendants, and works with
both the `hash` and `timestamp` source strategies:

```gitignore
# Ignore generated files and directories
*.generated.rs
src/generated/

# Keep one generated file tracked
!src/generated/schema.generated.rs
```

Patterns in nested `.octaignore` files override matching rules inherited from parent directories. As with
`.gitignore`, a file cannot be re-included if one of its parent directories is still ignored. Sources outside the
root `Octafile` directory are not filtered by `.octaignore`.

By default, Octa calculates file checksums, but you can switch it to track file modification
timestamps by setting `source_strategy: timestamp` at either the Octafile or task level.

By default, Octa stores all the necessary information for tracking sources in the `.octa` directory.
You can override this directory by setting the `OCTA_CACHE_DIR` environment variable.

If you still want the task to run even though the source files have not changed, you can use 
the `--force` or `-f` flag.

## Watch mode

Use `--watch` or `-w` to run a task immediately and rerun it whenever a source file is created,
modified, or removed. At least one task in the execution plan must define `sources`.

```console
octa --watch build
```

Watch mode applies glob expansion and `.octaignore` rules in the same way as regular source
fingerprinting. When multiple commands are selected, a change reruns all of them. A failed run does
not stop the watcher; Octa waits for the next source change. Press Ctrl-C to stop watching.

A task can enable watch mode when it is selected directly from the command line:

```yaml
version: 1

interval: 500ms

tasks:
  build:
    watch: true
    sources:
      - src/**/*.rs
    shell: cargo build
```

The default interval is `100ms`. Set `interval` at the root of the Octafile or override it from the
command line with `--interval 1s`. Durations must be positive integers ending in `ms`, `s`, or `m`.

## Task conditions
Use the `if` parameter to run a task only when a condition command exits successfully. A non-zero
exit code skips the guarded work without failing the execution plan. A string is shorthand for a
condition handled by the effective `default_plugin` and runs once after dependencies, so it can use
task variables, environment variables, and `deps_result`. A one-plugin mapping can select any
registered plugin task type explicitly; it has the same default `after_deps`/`once` behavior.

```yaml
version: 1

vars:
  ENVIRONMENT: development

tasks:
  deploy:
    if: test "{{ ENVIRONMENT }}" = "production"
    shell: ./deploy.sh

  prepare: ./prepare.sh

  plugin-condition:
    if:
      tpl: condition passed
    shell: ./plugin-guarded.sh

  pipeline:
    if:
      before_deps: test -f config.yml
      after_deps:
        shell: test "$(df -Pk . | awk 'NR == 2 { print $4 }')" -gt 1048576
        evaluate: per_command
    deps:
      - prepare
    cmds:
      - shell: ./build.sh
      - shell: ./package.sh
        if: test -n "$SIGNING_KEY"
      - shell: ./optional-check.sh
        ignore_error: true
      - task: deploy
        silent: true
```

The phased form accepts `before_deps` and `after_deps`. String values use `evaluate: once` and
share one result with all commands in that task invocation. `after_deps` also accepts a detailed
form with `evaluate: per_command`, which re-runs the condition immediately before every executable
command. This is useful for checks whose result can change while the task is running, such as free
disk space. `before_deps` only supports `once`, because it gates the dependency subtree as a whole.
In a detailed condition, `shell` is a plugin task type rather than a field interpreted by the
executor; another registered plugin key can be used in its place.

The condition is still evaluated when `--force` is used. In dry mode, Octa prints the task commands
without executing the condition and treats it as successful.

`if`, `silent`, and `ignore_error` can also be set on an individual command or task reference. Use
the mapping form shown above when a command needs options. A command-level `if` is an additional
condition and accepts either the string shorthand or a one-plugin mapping. `silent` and
`ignore_error` override task-level defaults when present. These options are handled by Octa and
work with every plugin command type.

## Task preconditions
Sometimes you need to check a condition before executing a task and decide whether to run it or not.
To do this, you can specify the `preconditions` parameter for the task and list all the necessary checks there.
In preconditions, you can use expand syntax as in variables, and you also have access to the results of subtasks.

```yaml
version: 1

tasks:
  hello:
    shell: echo Hello
    preconditions:
      - "{{ deps_result.build == 'true' }}"
    deps:
      - build

  build:
    shell: echo true
```

# Plugins
Information about plugins you can find in plugins documentation [here](https://github.com/OctaHive/octa/blob/main/docs/plugins.md)
