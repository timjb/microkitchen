# Getting started

microkitchen launches [microsandbox](https://microsandbox.dev) microVMs from a
[mise](https://mise.jdx.dev) `mise.toml`: Docker inside, tools installed with
`mise bootstrap`, environment and secrets resolved on the host, and an egress
broker that asks before the sandbox talks to anything new.

```console
$ microkitchen
   ✓ Created      mk-api-1a2b3c4d (41.2s)
   ✓ Started      Docker (27.3s)
running mise bootstrap (log: ~/.microkitchen/logs/mk-api-1a2b3c4d/bootstrap.log)
root@mk-api-1a2b3c4d:~#
```

::: warning Pre-release
microkitchen is at 0.1.0. It is developed and tested on Linux with KVM. The
macOS dialog backend is implemented but untested.
:::

## Requirements

- Linux on x86_64 with KVM (`/dev/kvm`), or macOS on Apple Silicon (untested).
- [microsandbox](https://microsandbox.dev) **0.7.2** (the `msb` CLI and its
  runtime; the version must match the SDK microkitchen is built with).
- [mise](https://mise.jdx.dev) on `PATH`, used to discover the configuration and
  resolve environment variables on the host.
- For approval dialogs: `zenity` or `kdialog` on Linux (with a display), or
  macOS's built-in `osascript`. `notify-send` is used for notifications when
  present. Without a dialog, approvals fall back to the command line (see
  [Headless use](./network#headless-use)).
- Rust (edition 2024) to build, plus a C toolchain and `libcap-ng`'s development
  headers (`libcap-ng-dev` on Debian/Ubuntu, `libcap-ng-devel` on Fedora),
  needed to link microsandbox's krun-based VMM backend.

## Install

```sh
# microsandbox, pinned to the version microkitchen is built against
curl -fsSL -o install.sh \
  https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/install.sh
sed -i 's/^    get_latest_version$/    VERSION=v0.7.2/' install.sh && sh install.sh

# microkitchen, from a clone of the repository
cargo install --locked --path crates/microkitchen   # or: just install
```

The microsandbox installer otherwise installs the latest release; the `sed`
pins it (the CI workflow does the same). On macOS, write `sed -i ''` instead of
`sed -i`.

## Quick start

Add a `[_.microkitchen]` table to a project's `mise.toml`:

```toml
[tools]
node = "22"

[env]
GITHUB_TOKEN = { required = true }

[_.microkitchen]
cpus = 4
memory = "8G"
mounts = ["./:/work"]

[_.microkitchen.network]
allow = ["registry.npmjs.org", "*.github.com"]

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]
```

Then run `microkitchen` in the project. It:

1. finds the kitchen file the way mise finds its configuration,
2. validates it and resolves `[env]` on the host with mise,
3. creates the sandbox (or starts the existing one) from
   `cruizba/ubuntu-dind:noble-latest`, with Docker running inside,
4. runs `mise bootstrap` once, with the network open so tools can install,
5. from then on mediates the sandbox's network access, and
6. attaches a shell as the sandbox user, [chef](./chef).

Run it again to get back into the same sandbox; `microkitchen down` removes it.

## Next steps

- [Configuration](./configuration): every key of the `[_.microkitchen]` table.
- [Network access](./network): how connections are decided and approved.
- [Commands](../reference/commands): everything the CLI can do.
