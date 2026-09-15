# Task-result cache example

This example needs no cache profile. From this directory, run:

```console
octa build
octa cache explain build
```

The first command creates `dist/message.txt` and publishes the complete task
result below `.octa/cache`. `cache explain` then reports a local hit.

Delete `dist` and run `octa build` again. Octa restores the directory from the
cache and replays the stored standard output without executing the shell step.
Changing `input/message.txt` produces a different action key and executes the
step again.

The automatic cache is intended for trusted local development. It separates
Octa versions, operating systems, and CPU architectures, but cannot discover
every compiler or SDK invoked by an opaque shell command. Use an explicit
[`cache-profile.toml`](../../docs/cache-profile.md) with a maintained
`environment.identity` for toolchain-sensitive CI or a shared remote cache.
