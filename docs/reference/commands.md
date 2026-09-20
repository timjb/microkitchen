# Commands

| Command | What it does |
|---|---|
| `microkitchen` / `up [--no-shell] [--recreate]` | Create or start the sandbox, bootstrap it, attach a shell. `--recreate` replaces it (only the mise cache is kept). |
| `shell [--root]`, `exec [--root] -- <cmd>` | Attach a shell / run a command in the sandbox, as chef or, with `--root`, as root. |
| `stop`, `start`, `restart` | Lifecycle. Use `microkitchen start`, not `msb start` (see [Limitations](../guide/network#limitations)). |
| `down [--purge]` | Remove the sandbox; `--purge` also deletes its state and logs (never the shared mise cache). |
| `status`, `list` (`ls`) | This project's sandbox; all sandboxes microkitchen created. |
| `logs [--bootstrap \| --broker \| --sandbox]` | The bootstrap log (default), the broker's log, or the sandbox's own output. |
| `bootstrap` | Run `mise bootstrap` again, e.g. after a failed install (`remodel` runs it after tool changes). |
| `validate` | Check the configuration and resolve the environment without creating anything. |
| `remodel [--yes] [--recreate]` | Apply configuration changes to the existing sandbox ([details](../guide/remodel)). |
| `net …` | Network rules and approvals ([details](../guide/network#rules-and-commands)). |
| `broker start \| stop \| status` | The egress broker daemon (started automatically when needed). |

## Global options

- `-C <dir>`: run as if in `<dir>`.
- `--home <dir>`: the state directory, default `~/.microkitchen`, or
  `MICROKITCHEN_HOME`.
- `-v` / `-q`: more or less output.
- `--json`: machine-readable output where supported.

## Progress output

Pulling the image, creating, starting, stopping and removing the sandbox, and
non-interactive `exec`, show a progress spinner on stderr when it is a
terminal (with per-layer bars while the image is pulled). Finished steps leave
a `✓` line; `exec` leaves only the command's own output.
