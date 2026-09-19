# microkitchen

Launch [microsandbox](https://microsandbox.dev) microVMs from a
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

Status: pre-release (0.1.0). Developed and tested on Linux with KVM. The macOS
dialog backend is implemented but untested.

## Contents

- [Requirements](#requirements)
- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [The sandbox user: chef](#the-sandbox-user-chef)
- [Environment variables and secrets](#environment-variables-and-secrets)
- [Network access](#network-access)
- [Changing a sandbox: `remodel`](#changing-a-sandbox-remodel)
- [Settings](#settings)
- [Files](#files)
- [mise caveats](#mise-caveats)
- [Development](#development)

## Requirements

- Linux on x86_64 with KVM (`/dev/kvm`), or macOS on Apple Silicon (untested).
- [microsandbox](https://microsandbox.dev) **0.6.18** (the `msb` CLI and its
  runtime; the version must match the SDK microkitchen is built with).
- [mise](https://mise.jdx.dev) on `PATH`, used to discover the configuration and
  resolve environment variables on the host.
- For approval dialogs: `zenity` or `kdialog` on Linux (with a display), or
  macOS's built-in `osascript`. `notify-send` is used for notifications when
  present. Without a dialog, approvals fall back to the command line (see
  [Headless use](#headless-use)).
- Rust (edition 2024) to build, plus a C toolchain and `libcap-ng`'s development
  headers (`libcap-ng-dev` on Debian/Ubuntu, `libcap-ng-devel` on Fedora),
  needed to link microsandbox's krun-based VMM backend.

## Install

```sh
# microsandbox, pinned to the version microkitchen is built against
curl -fsSL -o install.sh \
  https://github.com/superradcompany/microsandbox/releases/download/v0.6.18/install.sh
sed -i 's/^    get_latest_version$/    VERSION=v0.6.18/' install.sh && sh install.sh

# microkitchen
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
6. attaches a shell as the sandbox user, [chef](#the-sandbox-user-chef).

Run it again to get back into the same sandbox; `microkitchen down` removes it.

## Commands

| Command | What it does |
|---|---|
| `microkitchen` / `up [--no-shell] [--recreate]` | Create or start the sandbox, bootstrap it, attach a shell. `--recreate` replaces it (only the mise cache is kept). |
| `shell [--root]`, `exec [--root] -- <cmd>` | Attach a shell / run a command in the sandbox, as chef or, with `--root`, as root. |
| `stop`, `start`, `restart` | Lifecycle. Use `microkitchen start`, not `msb start` (see [Limitations](#limitations)). |
| `down [--purge]` | Remove the sandbox; `--purge` also deletes its state and logs (never the shared mise cache). |
| `status`, `list` | This project's sandbox; all sandboxes microkitchen created. |
| `logs [--bootstrap \| --broker \| --sandbox]` | The bootstrap log (default), the broker's log, or the sandbox's own output. |
| `bootstrap` | Run `mise bootstrap` again, e.g. after a failed install (`remodel` runs it after tool changes). |
| `validate` | Check the configuration and resolve the environment without creating anything. |
| `remodel [--yes] [--recreate]` | Apply configuration changes to the existing sandbox ([details](#changing-a-sandbox-remodel)). |
| `net …` | Network rules and approvals ([details](#rules-and-commands)). |
| `broker start \| stop \| status` | The egress broker daemon (started automatically when needed). |

Global options: `-C <dir>` (run as if in `<dir>`), `--home <dir>` (state
directory, default `~/.microkitchen`, or `MICROKITCHEN_HOME`), `-v`/`-q`, and
`--json` for machine-readable output where supported.

Pulling the image, creating, starting, stopping and removing the sandbox, and
non-interactive `exec`, show a progress spinner on stderr when it is a
terminal (with per-layer bars while the image is pulled). Finished steps leave
a `✓` line; `exec` leaves only the command's own output.

## Configuration

microkitchen reads the **kitchen file**: the highest-precedence file among those
mise loads that contains a `[_.microkitchen]` table (falling back to the nearest
`mise.toml`). Approval answers and `net allow|deny` are written to it.

```toml
[_.microkitchen]
cpus   = 2                  # 1–64                                   default 2
memory = "4G"               # up to 64G                              default "4G"
disk   = "10G"              # root disk (flat ext4)                  default "10G"
mounts = ["./src:/app", "./data:/data:ro"]   # host:guest[:ro]

[_.microkitchen.network]
network = "public"          # none | public | open                   default "public"
allow   = ["example.com", "*.microsandbox.dev", "203.0.113.7"]
deny    = ["potentiallymalicious.com"]
ports   = ["8000:8000", "9100:9100/udp"]     # host:guest[/udp]

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]       # hosts that receive the real value
```

- **Sizes**: `M`/`MB`/`MiB`/`G`/`GB`/`GiB` (case-insensitive); a bare number is
  MiB.
- **Mounts**: the host path is relative to the kitchen file's directory and
  must exist. Guest paths must be absolute, unique, and must not overlap
  `/root/.cache/mise`, `/root/kitchen` or `/.msb`.
- **Ports** are published on the host's `127.0.0.1` only.
- **`network`** is microsandbox's preset: `none` disables networking, `public`
  blocks private ranges, loopback, link-local and cloud metadata, `open` allows
  everything microsandbox allows. `allow`/`deny` are enforced by microkitchen's
  broker, not by microsandbox ([Network access](#network-access)).
- **Rule entries** (`allow`, `deny`, secret `allow`): an exact host name, a
  `*.suffix` (the suffix and all its subdomains), an IPv4/IPv6 address, or a
  CIDR range. Entries are validated, never repaired.
- A plain `[microkitchen]` table also works, but mise warns about it; use
  `[_.microkitchen]`, which mise ignores by design. Having both is an error.

The configuration is validated before every create or change, with every
problem reported at its line. `microkitchen validate` runs the same checks.

## The sandbox user: chef

Shells and commands run as `chef`, not root. Unless the kitchen file declares
chef itself, the sandbox gets:

```toml
[bootstrap.users.chef]
uid = 1001
group = "chef"
groups = ["sudo", "docker"]   # passwordless sudo, and Docker
shell = "/bin/bash"
comment = "sandbox user"

[bootstrap.groups.chef]
gid = 1001
```

The `sudo` group may use sudo without a password. `exec --root` and
`shell --root` run as root directly.

To change chef, declare `[bootstrap.users.chef]` yourself; it is used as
written (see mise's [accounts](https://mise.jdx.dev/bootstrap/accounts.html)),
except that a missing `uid` is 1001 and a missing `group` is `chef` (with gid
1001). Leave `sudo` out of `groups` and chef has no sudo:

```toml
[bootstrap.users.chef]
groups = ["docker"]
shell = "/bin/sh"
```

chef's uid and gid are fixed when the sandbox is created: the sandbox runs as
them before bootstrap has created chef, and files in `mounts` appear owned by
them, so chef can write there. Changing them needs `remodel --recreate`, and a
primary group other than `chef` must declare its `gid`. chef cannot be removed
(`state = "absent"`) or be uid 0.

## Environment variables and secrets

`[env]` is resolved **on the host** with mise (`_.file`, `_.source`, templates
and all). Each resolved variable goes into the sandbox:

- as a **plain environment variable**, unless
- a `[_.microkitchen.secrets.NAME]` table exists for it: then it is a
  microsandbox **secret**. The guest sees only a placeholder; the real value is
  substituted into TLS connections to the secret's `allow` hosts, and the
  placeholder passes through unchanged everywhere else.

Every secret table needs a matching `[env]` declaration. Variables that resolve
to an empty string are not injected; a secret declared for one is skipped with a
notice. The guest's copy of `mise.toml` drops `_.file`, `_.source` and `_.path`,
so `.env` files never enter the sandbox.

## Network access

Every sandbox's DNS and outbound traffic goes through **the egress broker**, a
per-user daemon (`microkitchen broker`). It gives each sandbox its own DNS
resolver, which records which names the sandbox resolved to which addresses,
and its own SOCKS5 proxy for TCP and UDP. microsandbox refuses DNS to any other
resolver and DNS over TLS, so the broker sees the sandbox's name lookups.

### How a connection is decided

In order, first match wins:

1. Cloud metadata, link-local, multicast, unspecified and broadcast addresses:
   always denied.
2. While `mise bootstrap` runs: everything else is allowed.
3. **The kitchen file's rules**, then **`~/.microkitchen/rules.toml`** (for
   every sandbox). Within each file, `deny` wins over `allow`; each rule is
   checked against the address and the names the sandbox resolved to it. A
   name rule never matches an address the sandbox did not look up for that
   name. `ntp.ubuntu.com` is allowed built in (the image's time sync).
4. Temporary allows ("Allow 5 min", `net temp`).
5. Otherwise: **ask**.

### Approvals

The dialog shows the sandbox, the destination, the names the sandbox resolved
for it (or a warning that it never resolved this address, typical of hard-coded
addresses), and the process that opened the connection:

```
Sandbox:      mk-api-1a2b3c4d
Destination:  140.82.121.3 : 443  (TCP)
Resolved as:  api.github.com
Process:      pid 412 (node)
```

- **Deny** adds the name (or address) to the kitchen file's `deny`.
- **Allow** adds it to `allow`.
- **Allow 5 min** allows it for this sandbox for five minutes, not saved.
- Closing the dialog, or no answer within 60 seconds, denies this connection
  only and saves nothing.

One dialog is shown at a time, across all sandboxes; identical concurrent
requests share one dialog, and a request whose connection is gone is dropped
unseen. The process line is for information only: it comes from inside the
guest and never decides anything. It shows the short process name, never the
command line.

**Rate limit**: more than 20 prompts within 10 minutes switch that sandbox to
deny-all (even allowed names), with one notification. It stays that way, across
broker restarts, until `microkitchen net resume`.

### Headless use

Without a desktop (`approval.dialog`, see [Settings](#settings)), the fallback
is `approval.headless`:

- `"deny"` (default): anything that would ask is denied.
- `"queue"`: requests wait for an answer from `microkitchen net pending` and
  `microkitchen net decide <id> allow|deny|temp`.

`net pending` / `net decide` also work while a dialog is up.

### Rules and commands

| Command | |
|---|---|
| `net allow <rule> [--global]`, `net deny <rule> [--global]` | Add to the kitchen file (or `~/.microkitchen/rules.toml`); moves it out of the other list. |
| `net revoke <rule> [--global]` | Remove it from both lists. |
| `net rules` | The rules in effect, including built-ins and global rules. |
| `net temp <host>` | Allow for five minutes, this sandbox only. |
| `net pending`, `net decide <id> allow\|deny\|temp` | Headless approvals. |
| `net resume` | Leave deny-all after the rate limit tripped. |
| `net bindings` | Which names the sandbox resolved to which addresses. |
| `net mode open\|enforce` | Switch mediation off or on for this sandbox. |

Rule changes, by command or by editing either file, apply to the next
connection; nothing restarts. `~/.microkitchen/rules.toml` holds top-level
`allow = [...]` and `deny = [...]` lists in the same syntax.

### Limitations

- **The broker is in the path.** While it is not running, sandboxes have no
  network access at all. `up`, `start`, `exec` and the other commands that start
  a sandbox restart it. After a restart, saved rules apply at once; bindings and
  temporary allows are gone, so the first connections may ask again.
- **DNS over HTTPS** cannot be blocked by port. A program that resolves names
  that way connects to addresses the sandbox never resolved, and the dialog
  says exactly that.
- **UDP attribution** works only for connected sockets; others show no process.
- **`msb start <name>`** outside microkitchen fails proxy authentication: the
  proxy password is set by microkitchen when it starts a sandbox.

## Changing a sandbox: `remodel`

After editing the kitchen file, `microkitchen remodel` lists what changed
compared with what the sandbox runs with, says when each change takes effect,
and asks before applying (`--yes` skips the question):

| Change | Takes effect |
|---|---|
| `allow` / `deny` | At once (the broker re-reads the file). |
| `cpus`, `memory` | At once when the runtime can resize the running VM, otherwise after `microkitchen restart`. |
| `disk`, environment variables, secrets | After `microkitchen restart` (at the next start for a stopped sandbox). |
| `mounts`, `ports`, `network`, chef's `uid` or `gid` | Need a new sandbox: `microkitchen remodel --recreate` (only the mise cache is kept). |

When anything mise uses changes (tools, `[env]`, `[bootstrap]` and so on),
`remodel` also updates the guest's `mise.toml` and runs `mise bootstrap` to
apply it; a stopped sandbox is started for this and stopped again. While a
change to chef's ids waits for `--recreate`, so does the guest's `mise.toml`. It only ever removes environment variables that
came from the kitchen file, never the image's own.

## Settings

`~/.microkitchen/config.toml` (every key optional):

```toml
mise_version = "2026.9.6"      # mise installed in guests (default: latest)

[approval]
dialog = "auto"                # auto | zenity | kdialog | osascript | none
headless = "deny"              # no dialog: "deny" or "queue" for `net decide`
timeout_secs = 60              # unanswered approvals deny the connection
max_prompts = 20               # more prompts than this within window_secs
window_secs = 600              #   switch a sandbox to deny-all

[broker]
port_range = [40000, 49999]    # per-sandbox resolver and proxy ports
upstream_dns = ["1.1.1.1"]     # default: the host's /etc/resolv.conf
```

`dialog = "auto"` uses `osascript` on macOS and, when `DISPLAY` or
`WAYLAND_DISPLAY` is set, `zenity` or `kdialog` on Linux.

## Files

```
~/.microkitchen/
├── config.toml                  settings
├── rules.toml                   network rules for every sandbox
├── sandboxes/<name>/            state.json (applied configuration), proxy-secret
├── logs/<name>/bootstrap.log    mise bootstrap output
├── logs/broker.log              broker log
└── broker/                      audit.log (one JSON line per decision), registry.json
```

Sandboxes are named `mk-<directory>-<hash of the kitchen file path>` and carry
`microkitchen.*` labels linking them to their kitchen file. mise's cache lives
in the `microkitchen-mise-cache` volume, shared by all kitchens.

## mise caveats

- mise rejects `{ required = false }`. Declare optional variables as
  `{ default = "" }`; they are injected only when set on the host.
- Variables that mise passes through unchanged from the host environment (for
  example `{ required = true }` ones) are read from the host directly.
- `[tools]` are installed by `mise bootstrap` inside the guest when the
  sandbox is created and again by `microkitchen remodel` after they change.
- Inside the guest the kitchen file is `/opt/kitchen/mise.toml` (without the
  `[_.microkitchen]` table, and with chef added if missing). It is mise's
  system config, so tools work from any directory (mounts included), and
  chef's global config (`~/.config/mise/config.toml`) stays chef's own for
  `mise use -g`.
- `mise bootstrap` runs in two steps. Root installs mise and sudo and applies
  the accounts, which creates chef; then chef runs the full bootstrap, so
  `[tasks.bootstrap]` runs as chef. Tools are installed in `/opt/mise`, owned
  by chef, and mise's cache is `/var/cache/mise`.

## Development

```sh
just test              # unit tests and VM-free integration tests
just lint              # rustfmt and clippy, warnings are errors
just check             # both (what CI runs on hosted runners)
just test-scripts      # the guest attribution script against fake /proc trees
just test-integration  # VM tests: need KVM, msb and cargo-nextest
just test-vm           # VM tests with cargo test, one at a time
```

The VM tests boot real sandboxes against the internet; with
`MK_TEST_ISOLATE_HOME=1` they use their own microsandbox home. Their kitchens
use one vCPU: in nested virtualization, 2-vCPU guests were several times slower
to boot and bootstrap.

Design and plan: [`specs/egress-broker-design.md`](specs/egress-broker-design.md),
[`specs/implementation-plan.md`](specs/implementation-plan.md).

## License

Apache-2.0
