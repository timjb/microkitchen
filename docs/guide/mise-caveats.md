# mise caveats

- mise rejects `{ required = false }`. Declare optional variables as
  `{ default = "" }`; they are injected only when set on the host.
- Variables that mise passes through unchanged from the host environment (for
  example `{ required = true }` ones) are read from the host directly.
- `[tools]` are installed by `mise bootstrap` inside the guest when the
  sandbox is created and again by [`microkitchen remodel`](./remodel) after
  they change.
- Inside the guest the kitchen file is `/opt/kitchen/mise.toml` (without the
  `[_.microkitchen]` table, and with [chef](./chef) added if missing). It is
  mise's system config, so tools work from any directory (mounts included), and
  chef's global config (`~/.config/mise/config.toml`) stays chef's own for
  `mise use -g`.
- `mise bootstrap` runs in two steps. Root installs mise and sudo and applies
  the accounts, which creates chef; then chef runs the full bootstrap, so
  `[tasks.bootstrap]` runs as chef. Tools are installed in `/opt/mise`, owned
  by chef, and mise's cache is `/var/cache/mise`.
