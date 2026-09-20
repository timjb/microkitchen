# Configuration

microkitchen reads the **kitchen file**: the highest-precedence file among those
mise loads that contains a `[_.microkitchen]` table (falling back to the nearest
`mise.toml`). Approval answers and `net allow|deny` are written to it.

```toml
[_.microkitchen]
cpus   = 2                  # 1–64                                   default 2
memory = "4G"               # up to 64G                              default "4G"
disk   = "10G"              # root disk (flat ext4)                  default "10G"
mounts = ["./src:/app", "./data:/data:ro"]   # host:guest[:ro]
dotfiles = "~/.dotfiles"    # staged as mise's dotfiles.root       default none

[_.microkitchen.network]
network = "public"          # none | public | open                   default "public"
allow   = ["example.com", "*.microsandbox.dev", "203.0.113.7"]
deny    = ["potentiallymalicious.com"]
ports   = ["8000:8000", "9100:9100/udp"]     # host:guest[/udp]

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]       # hosts that receive the real value
```

## Sizes

`M`/`MB`/`MiB`/`G`/`GB`/`GiB` (case-insensitive); a bare number is MiB.

## Mounts

The host path is relative to the kitchen file's directory and must exist.
Guest paths must be absolute, unique, and must not overlap
`/var/cache/mise`, `/opt/kitchen`, `/opt/mise` or `/.msb`, which microkitchen
manages.

## `dotfiles`

A host directory staged into the sandbox as mise's `dotfiles.root`, so
`[dotfiles]` entries without a `source` resolve. See
[Dotfiles and system files](./dotfiles).

## Ports

Ports are published on the host's `127.0.0.1` only.

## `network`

microsandbox's preset: `none` disables networking, `public` blocks private
ranges, loopback, link-local and cloud metadata, `open` allows everything
microsandbox allows. `allow`/`deny` are enforced by microkitchen's broker, not
by microsandbox (see [Network access](./network)).

## Rule entries

`allow`, `deny` and a secret's `allow` take an exact host name, a `*.suffix`
(the suffix and all its subdomains), an IPv4/IPv6 address, or a CIDR range.
Entries are validated, never repaired.

## Table name

A plain `[microkitchen]` table also works, but mise warns about it; use
`[_.microkitchen]`, which mise ignores by design. Having both is an error.

## Validation

The configuration is validated before every create or change, with every
problem reported at its line. `microkitchen validate` runs the same checks.
