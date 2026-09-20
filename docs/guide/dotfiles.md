# Dotfiles and system files

mise can place files on a machine during setup, and microkitchen runs
`mise bootstrap` in every sandbox, so both of its file features work in a
kitchen:

- [dotfiles](https://mise.jdx.dev/dotfiles.html) — `[dotfiles]`, for a user's
  own configuration;
- [system files and directories](https://mise.jdx.dev/bootstrap/files.html) —
  `[bootstrap.files]` and `[bootstrap.directories]`, for absolute paths like
  `/etc`.

```toml
[dotfiles]
"~/.gitconfig" = { source = "dotfiles/gitconfig", mode = "copy" }
"~/.config/nvim" = { source = "dotfiles/nvim", mode = "symlink" }
"~/.netrc" = { content = "machine example.com\n" }

[bootstrap.files."/etc/apt/apt.conf.d/99custom"]
source = "etc/apt.conf"
owner = "root"
mode = "0644"
```

Entries are applied by `mise bootstrap` inside the sandbox, in mise's own
order: system files around package installation, dotfiles after. microkitchen's
part is getting the host files they name into the guest first.

## Staging

Only the kitchen file itself reaches the sandbox, as
`/opt/kitchen/mise.toml`. Any `source` in it would therefore point at nothing,
so microkitchen **stages** the host files an entry names — it copies exactly
those paths into the sandbox and rewrites each `source` to its copy.

This is the same move mise makes for [remote bootstrap over
SSH](https://mise.jdx.dev/bootstrap/remote.html), which archives the source
directory and unpacks it on the target, narrowed to the paths the configuration
actually names. Nothing else from the project is copied.

Staged files land under `/opt/kitchen/files`:

```
/opt/kitchen/
├── mise.toml            the kitchen file, owned by root
└── files/               staged sources, owned by chef
    ├── project/…        sources under the kitchen file's directory
    └── <hash>/…         sources from anywhere else
```

Sources from outside the project are grouped by the host directory holding
them and named by a hash of it, so host user names and directory layout never
appear inside the sandbox — the same reason the guest's copy of the kitchen
file drops `_.file`, `_.source` and `_.path`.

`microkitchen validate` prints what will be copied, flagging anything from
outside the project:

```console
$ microkitchen validate
kitchen    /home/you/app/mise.toml  [_.microkitchen]
sandbox    mk-app-1a2b3c4d
stage      /home/you/app/dotfiles/gitconfig → project/dotfiles/gitconfig
stage      /home/you/shared/toolrc → 9f2ab01c/toolrc  (outside the project)
           staged under /opt/kitchen/files in the sandbox
```

## Sources are always host paths

A `source` names a file **on the host**, whether it is relative, `~/`-prefixed
or absolute. Relative ones resolve against the kitchen file's own directory,
as mise resolves them. A file that exists only in the sandbox's image cannot
be used as a source.

Sources may point outside the project — `../shared/gitconfig`, `~/.dotfiles` —
and are staged the same way. A source that does not exist, cannot be read, or
is a symbolic link with no target is an error, reported at its line before
anything is created.

In the sandbox every `source` is an absolute path under `/opt/kitchen/files`,
so nothing depends on how mise resolves relative paths for a system config.

### `dotfiles.root`

mise infers a source for entries that have none, under its `dotfiles.root`
setting. Point microkitchen at the host directory to use and it is staged like
any other source, with the sandbox's `dotfiles.root` set to the copy:

```toml
[dotfiles]
"~/.zshrc" = {}          # no source: inferred under dotfiles.root

[_.microkitchen]
dotfiles = "~/.dotfiles"
```

## Ownership, and editing in the sandbox

Staged files belong to [chef](./chef), so `mode = "symlink"` entries can be
edited through the link in the sandbox. Those edits change the sandbox's copy
only: **they are never written back to the host**, and they are lost when the
sandbox is recreated. To share a directory both ways, use a
[mount](./configuration#mounts) instead of a dotfile.

`/opt/kitchen/mise.toml` stays owned by root — it is mise's system config, not
something a sandbox process should rewrite.

`[bootstrap.files]` entries that need root work because chef has passwordless
sudo; a kitchen file that [takes chef's sudo away](./chef#customizing-chef)
cannot apply them.

## Applying changes

Editing a staged file changes no TOML, so microkitchen tracks the contents:
`microkitchen status` reports the sandbox as out of date and
`microkitchen remodel` copies the files in again and re-runs `mise bootstrap`.
Paths the kitchen file no longer references are removed from the sandbox.

```console
$ microkitchen remodel
changes:
  staged files           7 files, 4.1 KiB; copied into the guest, mise bootstrap applies them
```

`microkitchen bootstrap` also re-stages, so a failed bootstrap can be retried
after fixing a file.

## What microkitchen does not check

Whether an entry makes sense in a throwaway sandbox is mise's business, not
microkitchen's: it only reports what stops it producing a copy of a source.
Two consequences worth knowing:

- `mode = "track"` and `manifest = "git"` keep checkpoint history inside the
  sandbox, and it is lost with the sandbox. (`manifest = "git"` still works:
  the file list is resolved on the host, since the staged copy has no `.git`.)
- `[dotfiles]` in a *global* mise config on the host — `~/.config/mise/config.toml`
  — is never applied, because only the kitchen file reaches the sandbox. Move
  the entries into the kitchen file, or point `dotfiles` at the directory.

## Secrets in templates

`mode = "template"` renders in the sandbox, where a
[secret](./env-and-secrets) is only ever a placeholder — so a rendered file
holds the placeholder, and the real value never touches the sandbox's disk.
It is substituted on the wire, for that secret's allowed hosts only:

```toml
[env]
GITHUB_TOKEN = { required = true }

[dotfiles]
"~/.git-auth" = { source = "dotfiles/auth.tmpl", mode = "template" }

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]
```

Substitution is a literal match on the bytes sent, so this works only where the
value travels verbatim — `Authorization: Bearer <token>` does, a `~/.netrc`
that curl turns into `Authorization: Basic base64(user:token)` does not. See
[Environment variables and secrets](./env-and-secrets#secrets-in-files).

Staging a real credential file as a plain dotfile copies it into the sandbox in
clear text, outside the secret machinery entirely. Prefer a template and a
secret; `microkitchen validate` names every host file that will be copied.
