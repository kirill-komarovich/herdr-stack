# herdr-stack

A [Herdr](https://herdr.dev) plugin that runs a project's dev stack in the background: its docker
compose services and the dev servers you would otherwise keep open in terminal tabs. One daemon owns
them all. You drive it from a popup TUI, and agents drive it from a JSON CLI.

## Install

```sh
herdr plugin install kirill-komarovich/herdr-stack --ref v0.0.1
```

The install downloads the release binary for your platform (macOS or Linux, arm64 or x86_64). If there
is no release for the checked-out version, it builds from source with `cargo` instead.

Requires Herdr 0.8.0 or later. Docker units need `docker compose`, and `lsof` reports listening
ports.

A local unit gets the env a shell in its directory would have. Each start runs it through
`mise exec` and `direnv exec`, whichever are installed, so a restart picks up a changed `mise.toml`
or `.envrc`. direnv still wants `direnv allow` after an `.envrc` changes.

The popup and agents call `herdr-stack` by name, so put the installed binary on your `PATH`. It is
`target/release/herdr-stack` inside the plugin checkout. Then bind the popup in Herdr's `config.toml`:

```toml
[[keys.command]]
key = "prefix+d"
type = "popup"
command = "herdr-stack"
width = "90%"
height = "80%"
```

## Configure a project

Put a `.herdr-stack.toml` at the project root:

```toml
[local.rails]
cmd = ["bundle", "exec", "rails", "s"]
tty = true

[local.vite]
cmd = ["bin/vite", "dev"]

[docker]
names = ["db", "redis"]
one_shot = ["migrate"]
hidden = ["web"]

[includes.api]
path = "~/projects/api"
```

- `[local.<name>]` is a process herdr-stack spawns itself. `cmd` is an argv array; `cwd`, `env` and
  `tty` are optional. Set `tty = true` to type into the unit from its attach pane.
- `[docker]` lists compose service names: `names` can be started, `one_shot` run and exit, and
  `hidden` are never shown.
- `[docker.notes]` adds free text to a service's row.
- `[includes.<name>]` pulls in another repo's manifest, one level deep.
- `[env]` is merged under every unit's own `env`.

Ports never go in the manifest. herdr-stack reports the ports a unit is actually listening on.

## CLI

```sh
herdr-stack                                  # the popup TUI
herdr-stack status [--project <dir>]
herdr-stack start|stop|restart [--project <dir>] <unit>...
```

`status` and the verbs print JSON on stdout. Without `--project`, they use the nearest
`.herdr-stack.toml` at or above the working directory.

## Development

```sh
cargo build --release
herdr plugin link .
```

A linked plugin runs your working tree's `target/release/herdr-stack`, so rebuilding is enough.

`cargo test` runs everything except the tests against a real Herdr. Those start herdr servers of
their own, so they sit behind the `herdr-tests` feature and run only inside Docker. CI requires them
against the oldest supported Herdr and the latest:

```sh
tests/herdr/run.sh 0.8.0 0.9.3
```

To release, set the same version in `Cargo.toml` and `herdr-plugin.toml`, then push a matching `v*` tag.
CI builds the binaries and publishes them to a GitHub release.

## License

[MIT](LICENSE)
