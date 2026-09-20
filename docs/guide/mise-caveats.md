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
- Host files named by `[dotfiles]`, `[bootstrap.files]` and
  `[bootstrap.directories]` are [staged](./dotfiles) into
  `/opt/kitchen/files`, and every `source` in the guest's `mise.toml` is
  rewritten to its copy there. A `source` always names a host path, never one
  inside the sandbox.
- Only the kitchen file reaches the guest, so `[dotfiles]` declared in a global
  host config (`~/.config/mise/config.toml`) is not applied. Move the entries
  into the kitchen file, or set `[_.microkitchen] dotfiles`.
- `mise bootstrap` runs in two steps. Root installs mise and sudo and applies
  the accounts, which creates chef; then chef runs the full bootstrap, so
  `[tasks.bootstrap]` runs as chef. Tools are installed in `/opt/mise`, owned
  by chef, and mise's cache is `/var/cache/mise`.
