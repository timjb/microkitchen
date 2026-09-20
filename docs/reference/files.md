# Files

```
~/.microkitchen/
├── config.toml                  settings
├── rules.toml                   network rules for every sandbox
├── sandboxes/<name>/            state.json (applied configuration, staged digest), proxy-secret
├── logs/<name>/bootstrap.log    mise bootstrap output
├── logs/broker.log              broker log
└── broker/                      audit.log (one JSON line per decision), registry.json
```

Inside a sandbox, microkitchen owns `/opt/kitchen`:

```
/opt/kitchen/
├── mise.toml            the kitchen file as mise's system config, owned by root
└── files/               host files staged from [dotfiles] and [bootstrap.files]
    ├── project/…        sources under the kitchen file's directory
    └── <hash>/…         sources from anywhere else
```

`files/` is rewritten whenever the sources change; see
[Dotfiles and system files](../guide/dotfiles). Tools live in `/opt/mise` and
mise's cache in `/var/cache/mise`.

The directory can be changed with `--home <dir>` or `MICROKITCHEN_HOME`.
[`config.toml`](./settings) holds the settings; `rules.toml` holds the
[global network rules](../guide/network#rules-and-commands).

Sandboxes are named `mk-<directory>-<hash of the kitchen file path>` and carry
`microkitchen.*` labels linking them to their kitchen file. mise's cache lives
in the `microkitchen-mise-cache` volume, shared by all kitchens.
