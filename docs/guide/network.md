# Network access

Every sandbox's DNS and outbound traffic goes through **the egress broker**, a
per-user daemon (`microkitchen broker`). It gives each sandbox its own DNS
resolver, which records which names the sandbox resolved to which addresses,
and its own SOCKS5 proxy for TCP and UDP. microsandbox refuses DNS to any other
resolver and DNS over TLS, so the broker sees the sandbox's name lookups.

## How a connection is decided

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

## Approvals

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

::: info Rate limit
More than 20 prompts within 10 minutes switch that sandbox to deny-all (even
allowed names), with one notification. It stays that way, across broker
restarts, until `microkitchen net resume`.
:::

## Headless use

Without a desktop (`approval.dialog`, see [Settings](../reference/settings)),
the fallback is `approval.headless`:

- `"deny"` (default): anything that would ask is denied.
- `"queue"`: requests wait for an answer from `microkitchen net pending` and
  `microkitchen net decide <id> allow|deny|temp`.

`net pending` / `net decide` also work while a dialog is up.

## Rules and commands

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
`allow = [...]` and `deny = [...]` lists in the same syntax as the
[kitchen file's rules](./configuration#rule-entries).

## Limitations

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
