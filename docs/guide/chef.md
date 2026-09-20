# The sandbox user: chef

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

## Customizing chef

To change chef, declare `[bootstrap.users.chef]` yourself; it is used as
written (see mise's [accounts](https://mise.jdx.dev/bootstrap/accounts.html)),
except that a missing `uid` is 1001 and a missing `group` is `chef` (with gid
1001). Leave `sudo` out of `groups` and chef has no sudo:

```toml
[bootstrap.users.chef]
groups = ["docker"]
shell = "/bin/sh"
```

## User and group ids

chef's uid and gid are fixed when the sandbox is created: the sandbox runs as
them before bootstrap has created chef, and files in `mounts` appear owned by
them, so chef can write there. Changing them needs
[`remodel --recreate`](./remodel), and a primary group other than `chef` must
declare its `gid`. chef cannot be removed (`state = "absent"`) or be uid 0.
