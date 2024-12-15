# Inspiration
This project was inspired by the go-task build project. However, when we rewriting our project’s build system to go-task, I found some 
functionality missing, so I decided to create my own builder. 

# Installation

## Homebrew
If you have homebrew installed, you can install octa with:
```console

```

## Cargo
## Binary releases

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

After having a octa tasks file, you can run a tasks by calling `octa` and provide the names of the task to run. Provided tasks will 
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
toward the root directory. It follows a specific order, stopping as soon as it finds either a matching task, a octafile.{yml,yaml} 
file, or reaches the root directory with no further folders to check.
