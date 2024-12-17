[![codecov](https://codecov.io/github/OctaHive/octa/branch/main/graph/badge.svg?token=Q1UWZ4QHGZ)](https://codecov.io/github/OctaHive/octa)
![build](https://github.com/OctaHive/octa/actions/workflows/tests.yml/badge.svg)
![License: MIT](https://img.shields.io/github/license/adrianmrit/mom)

# Inspiration
This project was inspired by the go-task build project. However, when we rewriting our project’s build system to go-task, I found some 
functionality missing, so I decided to create my own builder. 

# The differences from go-task
* Support run nested tasks using wildcards
* Support return task result, this usefull for example when you need process task result in parent task
* Support rendering templates as a task result

# Installation

## Homebrew
If you have homebrew installed, you can install octa with:
```console


```

## Binary releases
Binaries are also available for Windows, Linux and macOS under [releases](https://github.com/OctaHive/octa/releases). To install, download the binary for 
your system and add to your `$PATH`.

# Getting started
Create a file called `Octafile.yml` in the root of your project and add your tasks to tasks section. In `cmds` section of task you should 
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

# Task files
The tasks are defined using the YAML format. So to start create your build tasks you need create task config file. 
The system currently supports configuration files in the following formats:

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

To run a task from your global Octafile located in your home directory, use the --global or -g flag. This is ideal for managing 
personal tasks that aren’t tied to a specific project.

You can also run tasks from a specific file by simply passing it with the `--config` or `-c` flag, e.g., `octa -c project_tasks.yml build`.

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
    cmd: echo {{ COMMAND_ARGS }}
```

```console
$ octa web -- publish
```

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

Octa also supports loading variables from `.env` files. The files are searched recursively, starting from the current directory.

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
  OBJ:
    val: 1
  ARR: ["A", "B", "C"]
  
tasks:
  say_hi:
    cmd: echo {{ STR }}
    
  plus_one:
    cmd: echo {{ NUM + 1 }}
    
  print_obj:
    cmd: echo {{ OBJ.val }}

  print_arr:
    cmd: echo {{ ARR[0] }}
```

You can use different data types as values. The following data types are supported:
* string
* bool
* number
* float
* array
* object

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
    cmd: echo Complex task
```

## Task command
Each task can have commands that will be executed in the command line (defaults to cmd in 
Windows and bash in Unix/Mac). There are two ways to set commands in a task: `cmd` and `cmds`. 
The cmds variant allows you to set multiple commands, which will be executed in sequence.

```yaml
version: 1
  
tasks:
  simple:
    cmd: echo Hello World!
    
  multiple:
    cmds:
      - echo Hello Alice!
      - echo Hello Bob!
    
```

### Task template
Sometimes you need to simply template text and return the result to a task that depends on the 
current one. To do this, you can specify a `tpl` for the task, and when the task is executed, 
the result will be templated using the specified variables and returned as the result of the task. 
This allows you to generate configurations, such as generating a docker-compose file for your project.

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
    cmd: echo "{{ deps_result['docker-compose-service'] }}"
    deps:
      - docker-compose-service
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

### Internal task
By default, all tasks defined in the file are available for execution via the command-line utility. 
Sometimes, it may be convenient to create a task that is only available internally, for example, if 
you need to call the same command with slight parameter variations. To achieve this, you can set the 
`internal` attribute for the task, making it unavailable for execution from the CLI utility and preventing 
it from appearing in the list of available tasks when using the `--list-tasks` command.

# Task directory
By default, tasks are executed in the directory where the Octafile is located. However, you can easily 
change the working directory for a task by specifying the `dir` parameter.

```yaml
version: 1

tasks:
  build:
    cmd: go build main.go
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
    cmd: go build main.go
    dir: "{{ USER_WORKING_DIR }}"
```

# Calling another task
In task commands, you can specify both shell commands and invoke other tasks. All commands listed within a 
task are executed sequentially by default, but you can change this behavior using the `execute_mode` parameter. 
This parameter supports two values: `parallel` and `sequentially`, with `sequentially` being the default.

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
    cmd: echo Start
  
  next:
    cmd: echo Finish
    
  run:
    cmds:
      - task: prev
      - echo Running task
      - task: next
```
