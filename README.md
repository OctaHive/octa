[![codecov](https://codecov.io/github/OctaHive/octa/branch/main/graph/badge.svg?token=Q1UWZ4QHGZ)](https://codecov.io/github/OctaHive/octa)
![build](https://github.com/OctaHive/octa/actions/workflows/tests.yml/badge.svg)
![License: MIT](https://img.shields.io/github/license/adrianmrit/mom)

# Inspiration
This project was inspired by the go-task build project. However, when we rewriting our project’s build system to go-task, I found some 
functionality missing, so I decided to create my own builder. 

# Installation

## Homebrew
If you have homebrew installed, you can install octa with:
```console


```

## Binary releases
Binaries are also available for Windows, Linux and macOS under [releases](https://github.com/OctaHive/octa/releases). To install, download the binary for 
your system and add to your `$PATH`.

# JSON scheme

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
<TBD>

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
