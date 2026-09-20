# Changing a sandbox: `remodel`

After editing the kitchen file, `microkitchen remodel` lists what changed
compared with what the sandbox runs with, says when each change takes effect,
and asks before applying (`--yes` skips the question):

| Change | Takes effect |
|---|---|
| `allow` / `deny` | At once (the broker re-reads the file). |
| `cpus`, `memory` | At once when the runtime can resize the running VM, otherwise after `microkitchen restart`. |
| `disk`, environment variables, secrets | After `microkitchen restart` (at the next start for a stopped sandbox). |
| `mounts`, `ports`, `network`, chef's `uid` or `gid` | Need a new sandbox: `microkitchen remodel --recreate` (only the mise cache is kept). |

Staged files are compared by content, so editing a dotfile counts as a change
even though the kitchen file is untouched; `remodel` copies them in again and
removes the ones the kitchen file no longer references (see
[Dotfiles and system files](./dotfiles)).

When anything mise uses changes (tools, `[env]`, `[bootstrap]` and so on),
`remodel` also updates the guest's `mise.toml` and runs `mise bootstrap` to
apply it; a stopped sandbox is started for this and stopped again. While a
change to chef's ids waits for `--recreate`, so does the guest's `mise.toml`.
It only ever removes environment variables that came from the kitchen file,
never the image's own.
