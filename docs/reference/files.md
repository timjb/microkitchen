# Files

```
~/.microkitchen/
├── config.toml                  settings
├── rules.toml                   network rules for every sandbox
├── sandboxes/<name>/            state.json (applied configuration), proxy-secret
├── logs/<name>/bootstrap.log    mise bootstrap output
├── logs/broker.log              broker log
└── broker/                      audit.log (one JSON line per decision), registry.json
```

The directory can be changed with `--home <dir>` or `MICROKITCHEN_HOME`.
[`config.toml`](./settings) holds the settings; `rules.toml` holds the
[global network rules](../guide/network#rules-and-commands).

Sandboxes are named `mk-<directory>-<hash of the kitchen file path>` and carry
`microkitchen.*` labels linking them to their kitchen file. mise's cache lives
in the `microkitchen-mise-cache` volume, shared by all kitchens.
