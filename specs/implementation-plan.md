# microkitchen — Implementation Plan

Companion to `initial-prompt.md` (product spec) and `egress-broker-design.md`
(network mediation design, referred to below as *the broker design*). Everything
here was checked against microsandbox 0.6.18 (SDK source, docs, integration
tests), mise 2026.9.6, and the `cruizba/ubuntu-dind` README.

Decisions recorded from the clarification rounds:

| Topic | Decision |
|---|---|
| Approval surface | Native desktop dialog, shown by a background daemon |
| Daemon topology | **One egress broker daemon serving all sandboxes** (broker design §"decided up front"); microkitchen is the orchestrator that registers sandboxes with it (answers broker design §16.1) |
| Project directory | Not mounted. Only `mise.toml` (with `_.file`/`_.source`/`_.path` stripped) is copied into the guest; explicit `mounts` only |
| Bootstrap networking | Sandbox is in *open* mode at the broker while `mise bootstrap` runs, then switches to *enforce* |
| `[..secrets.X].deny` | Does not exist. Secrets have `allow` only; unknown keys are a validation error |
| Config section name | `[_.microkitchen]`. mise documents top-level `_` as data it never parses (schema: `additionalProperties: true`), so no "unknown field" warning. Plain `[microkitchen]` is accepted with a hint to migrate |
| Optional env vars | `FIGMA_TOKEN = { default = "" }`. A variable that resolves to an empty string is treated as absent |
| Guest process attribution | POSIX shell script (`mk-whodial`) staged into the guest at boot, with the helper contract from broker design §8. A static binary is unnecessary here because the image is fixed (Ubuntu, has `sh`/`awk`) |

Where the product spec and the broker design overlap, the reconciliation is
spelled out in §7.6 (dialog buttons and where decisions are persisted).

---

## 1. Architecture

```
 host                                                                      microVM (microsandbox)
 ┌────────────────────────────────────────────────────────────────┐        ┌───────────────────────────┐
 │ microkitchen CLI (up / shell / remodel / net …)                │        │ cruizba/ubuntu-dind:noble │
 │   │ admin socket: register / retire / mode / decide / rules    │        │  dockerd  ◀── init script │
 │   ▼                                                            │        │  mk-whodial (sh script) ◀─┼── exec on demand
 │ EGRESS BROKER daemon (one for all sandboxes)                   │        │  mise bootstrap           │
 │   per-sandbox resolver endpoint 127.0.0.1:Rn ◀── DNS forwarder─┼────────┤  guest DNS                │
 │   per-sandbox proxy endpoint    127.0.0.1:Pn ◀── SOCKS5 client─┼────────┤  every TCP / UDP flow     │
 │   name observer → binding store → flow mediator → decision     │        │  /root/.cache/mise ◀ named│
 │   engine → approval queue → desktop dialog;  attributor        │        │                    volume │
 │ ~/.microkitchen/{config.toml, broker/, sandboxes/, logs/}      │        └───────────────────────────┘
 └────────────────────────────────────────────────────────────────┘
```

Why this shape (each point traces to a constraint in broker design §2):

- microsandbox natively routes all guest TCP (`CONNECT`) and non-DNS UDP
  (`UDP ASSOCIATE`) through an external SOCKS5 proxy, and its DNS forwarder
  accepts per-sandbox upstream `nameservers` as `IP:PORT`. The broker is both
  the proxy and the resolver for every sandbox, on distinct loopback ports, and
  the port a request arrives on *is* the sandbox identity (§4 of the design).
- The connection reaching the proxy is a fresh host-side connection, so the
  process behind it is recovered out of band: the guest socket stays in a
  connecting state while the broker holds the SOCKS reply, and a helper in the
  guest finds it in the kernel socket tables (§8 of the design).
- microsandbox evaluates its own policy before dialing the proxy, so it keeps
  the structural denies (private, loopback, link-local, metadata) and the broker
  owns the interactive per-name decisions (§11 of the design).

### Libraries (answer to "what does microsandbox use, can we use the same?")

microsandbox's network stack is a **smoltcp** userspace netstack with its own
TCP/UDP proxy tasks, **hickory** (`hickory-proto`/`hickory-net`) for DNS, rustls
for TLS interception, and **tokio-socks** as a SOCKS *client*. There is no SOCKS
server in it to reuse; the client side is what talks to us.

| Component | Choice | Reason |
|---|---|---|
| SOCKS5 server | Hand-rolled on tokio (RFC 1928 + RFC 1929 user/pass) | Needs exactly what the microsandbox client sends (`CONNECT`, `UDP ASSOCIATE`, user/pass auth) plus hooks the design demands: hold the reply during approval, per-datagram admission, reply-source filtering, fragment dropping, strict hostname grammar with no repair. `fast-socks5` would need to be forked for most of these. |
| DNS observer | `hickory-proto` for wire format (question, answers, CNAME chains, `HTTPS`/`SVCB` hints); upstreams from `resolv-conf` on Linux, `hickory-resolver`'s system config on macOS | Same technology as microsandbox; we only forward, never resolve. |
| Runtime | `tokio` multi-thread; `tokio_util::sync::CancellationToken` per flow | Cancellation must propagate from a closed control connection to a queued approval (design §14). |
| Secret compare | `subtle::ConstantTimeEq` | design §14 |
| Atomic writes | `tempfile::NamedTempFile::persist` | rule store and config edits |
| CLI | `clap` (derive) | |
| TOML | `toml` + `serde` for reading, `toml_edit` for writes that preserve comments | |
| Diff display | `similar` + `comfy-table` + `owo-colors` | remodel |
| Dialog | `zenity`/`kdialog` (Linux), `osascript` (macOS) as subprocesses | no portable Rust crate offers three labelled buttons plus a timeout |
| Guest helper | POSIX `sh` + `awk` script over `/proc`, installed via the SDK's `script()` mechanism | Image is fixed and ships both; no cross-build, no second binary. Same output contract as design §8 |
| Sandbox | `microsandbox = "0.6.18"` (default features) | |

---

## 2. Repository layout

```
Cargo.toml                       # workspace
crates/microkitchen/             # CLI + broker (one binary: `microkitchen`)
  bin/main.rs
  lib/lib.rs
  lib/cli/                       # one file per subcommand
  lib/config/                    # discover.rs, schema.rs, validate.rs, edit.rs, size.rs, hostpat.rs
  lib/mise/                      # env resolution, [env] declaration parsing, guest config rendering
  lib/sandbox/                   # naming/labels, build.rs, lifecycle.rs, bootstrap.rs, remodel.rs
  lib/broker/                    # daemon.rs, admin.rs, registry.rs, observer.rs, bindings.rs,
                                 # mediator/{socks5.rs,tcp.rs,udp.rs,grammar.rs}, decision.rs,
                                 # rules.rs, approval.rs, dialog/{zenity.rs,kdialog.rs,osascript.rs,headless.rs},
                                 # attributor.rs, ratelimit.rs, audit.rs
  lib/state/                     # ~/.microkitchen paths, state files, locks
  tests/                         # integration tests (§10)
crates/test-macros/              # #[mk_test]
crates/test-utils/               # isolated home, CLI runner, fixtures, broker test client
scripts/guest/whodial.sh         # process attribution helper baked into the sandbox
scripts/guest/tests/             # fixture /proc trees + shell test runner for whodial.sh
justfile                         # build, test, test-integration, test-scripts
```

`lib/` + `bin/` split and file-section conventions mirror the microsandbox
repo.

---

## 3. Configuration (`[_.microkitchen]` in mise.toml)

### Discovery

Same rules as mise: walk up from the cwd honouring mise's file precedence. We
run `mise config ls --json` (verified: prints loaded files in precedence order),
then:

- the **kitchen file** is the highest-precedence file containing a
  `[_.microkitchen]` table (fallback: the nearest `mise.toml`). Dialog decisions
  are written to this file.
- `[env]` declarations are the union of `[env]` tables across loaded files.

Verified mise behaviours:

- Top-level `_` is "a special key for information you'd like to put into
  mise.toml that mise will never parse"; the JSON schema marks it
  `additionalProperties: true`. `[_.microkitchen]` produces no warning. A plain
  `[microkitchen]` table works (mise warns) and microkitchen prints a migration
  hint; both present is a validation error.
- `{ required = false }` is rejected by mise. Optional variables use
  `{ default = "" }` (§4).

### Schema (all keys optional unless noted; `deny_unknown_fields` everywhere)

```toml
[env]
GITHUB_TOKEN = { required = true }   # mandatory: mise errors on the host if unset
FIGMA_TOKEN  = { default = "" }      # optional: forwarded only when non-empty

[_.microkitchen]
cpus   = 2            # u8, 1..=64            (default 2)
memory = "4G"         # size string → MiB     (default "4G", max 64G)
disk   = "10G"        # root disk (flat ext4) (default "10G")
mounts = ["./src:/app", "./data:/data:ro"]   # host:guest[:ro]; host relative to the kitchen file's dir

[_.microkitchen.network]
network = "public"    # none | public | open   (microsandbox preset; default public)
allow   = ["example.com", "*.microsandbox.dev", "203.0.113.7"]
deny    = ["potentiallymalicious.com"]
ports   = ["8000:8000", "9100:9100/udp"]      # host:guest[/udp]

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]        # non-empty, required

[_.microkitchen.secrets.FIGMA_TOKEN]          # skipped with a notice when FIGMA_TOKEN is empty
allow = ["api.figma.com"]
```

Sizes: `M`/`MB`/`MiB`/`G`/`GB`/`GiB` (case-insensitive) → MiB; bare integers
are MiB. Rule entries in `allow`/`deny`: exact host, `*.suffix` (suffix and its
subdomains), IPv4/IPv6 literal, CIDR. Names must satisfy the hostname grammar
(§7.3); they are validated, never repaired.

### Validation (before every create/modify; also `microkitchen validate`)

1. Structural: unknown keys, types, non-empty secret allow lists, valid rule
   entries, port/mount syntax, absolute guest paths, no duplicate guest targets
   or host ports, no entry in both `allow` and `deny`.
2. Limits: `cpus <= 64`, `memory <= 64G` (passed to the SDK as `max_cpus(64)`,
   `max_memory(64 GiB)` ceilings).
3. Secrets ⊆ env: every `[_.microkitchen.secrets.X]` needs `X` declared in a
   loaded `[env]` table (literal, `{ required = true }`, `{ default = "" }`,
   template).
4. Mount sources must exist on the host.
5. Host-side environment resolution succeeds (§4); mise's own message is shown
   for a missing required variable.

Errors are collected and reported together with file/line spans.

### Writing back

`config::edit` uses `toml_edit` to append to `[_.microkitchen.network].allow` or
`.deny` in the kitchen file (creating table/array if absent, preserving
comments, deduplicating), written atomically. A lock file in
`~/.microkitchen` serialises writers (CLI and broker).

---

## 4. Environment variables and secrets

Resolved on the host by mise:

```
MISE_AUTO_INSTALL=false mise -q -C <kitchen dir> env --json-extended
```

(Verified: with auto-install off, mise does not install `[tools]` to print the
environment.) We keep variables whose source is a loaded config file or an
`_.file`/`_.source`, and drop `PATH` and mise's own additions.

Two verified quirks drive the merge:

- **Pass-through omission.** A declared variable that already exists on the
  host and is left unchanged by mise (`{ required = true }`, or `{ default }`
  with the host value present) is *omitted* from `mise env` output. For every
  declared key missing from the output, microkitchen reads the host environment
  directly. A required key missing from both is already an error from mise.
- **Empty means absent.** `{ default = "" }` yields `""` when unset. Any
  variable resolving to the empty string is not injected, and a secret declared
  for it is skipped with a notice (`microkitchen validate` lists these).
  `remodel` re-evaluates, so setting it later adds the secret (restart
  required for new secrets, §9).

Per variable `K` with non-empty value `V`:

- `[_.microkitchen.secrets.K]` exists → SDK secret:
  ```rust
  .secret(|s| s.env("K").value(V)
      .allow_host("github.com").allow_host_pattern("*.github.com")
      .on_violation(|v| v.passthrough_all_hosts(true)))
  ```
  Guest sees a placeholder; substitution happens only on TLS connections to
  allowed hosts; the placeholder is forwarded unchanged elsewhere (passthrough,
  as specified). The SDK persists secret values in microsandbox's local store so
  restarts work without mise; `remodel` rotates values via `modify().secret()`.
- otherwise → `.env("K", V)`.

The guest copy of `mise.toml` keeps `[env]` (bootstrap inside the guest sees the
same declarations; required vars are satisfied by injected env/placeholders,
optional ones fall back to `default`) and `[_.microkitchen]`, but strips
`_.file`, `_.source`, `_.path` so `.env` values never enter the guest.

---

## 5. Sandbox construction

| Concern | SDK call | Notes |
|---|---|---|
| Name | `mk-<dir-slug>-<8 hex of sha256(abs kitchen path)>` | ≤128 bytes; stable per config path |
| Labels | `microkitchen.managed=true`, `microkitchen.config=<abs path>`, `microkitchen.config-hash`, `microkitchen.version`, `microkitchen.bootstrapped` | Lookup: `Sandbox::list_with(\|l\| l.label("microkitchen.config", path))` |
| Image + root disk | `.image_with(\|i\| i.oci("cruizba/ubuntu-dind:noble-latest").root_disk(disk_mib))` | Flat ext4 root so dockerd's overlay2 does not nest on the sandbox overlay (per the Docker-in-sandbox docs) |
| Resources | `.cpus(n).memory(mib).max_cpus(64).max_memory(64 GiB)` | ceilings enable live resize |
| Mounts | `.volume(guest, \|m\| m.bind(host)[.readonly()])` | |
| mise cache | `Volume::builder("microkitchen-mise-cache").create()` once; `.volume("/root/.cache/mise", \|m\| m.named(..))` | shared by all kitchens |
| Ports | `.port(h,g)` / `.port_udp(h,g)` | bind 127.0.0.1 |
| Network preset | `none` → `.disable_network()`; `public`/`open` → `.network(\|n\| n.policy(preset))` | `allow`/`deny` are **not** passed to microsandbox; the broker owns them (design §11) |
| Structural denies | `.prepend_network_policy_rules([deny egress tcp+udp 53, deny egress tcp 853])` | closes "guest talks to its own resolver" and DoT (design §11.1). The gateway forwarder is not affected: queries to the gateway are intercepted before policy |
| DNS | `.network(\|n\| n.dns(\|d\| d.nameservers(["127.0.0.1:<Rn>"])))` | Rn from `register` |
| Proxy | `.proxy(\|p\| p.socks5("127.0.0.1:<Pn>").credentials(<sandbox name>, SecretSource::env("MK_PROXY_SECRET_<hash>")))` | password read by microsandbox from the host env at every sandbox start; microkitchen sets that variable in its own process before `create`/`start` |
| Env/secrets | §4 | |
| Init | `.init("auto")` | Resolves to the image's `/sbin/init` (systemd); the image's enabled `docker.service` starts dockerd on every boot, and `up`/`start` wait for `docker info` (90 s). Verified in milestone 2: an `entrypoint`/startup command runs only at *create* (the SDK's launch intent is not persisted), processes backgrounded from `exec` keep the exec from returning, and handing PID 1 to `supervisord` breaks exec |
| Guest helper | `.script("mk-whodial", include_str!("scripts/guest/whodial.sh"))` | lands in `/.msb/scripts/mk-whodial` (on `PATH`) |
| Guest config | written after boot through `exec` (stdin → `/root/kitchen/mise.toml`), symlinked as mise's global config `/root/.config/mise/config.toml` | tools resolve from any directory (mounts included) and the global config needs no `mise trust`; guest `PATH` starts with mise's shims so `exec` and shells see tools without activation |
| Mode | `.create_detached()` | |

**Endpoints are immutable per sandbox**: `modify()` has no proxy/DNS fields, so
the broker must hand back the same ports on every `up`. `register` therefore
accepts the previously assigned ports (from `state.json`) and fails if they are
taken; `up` then reports it and offers `--recreate`. Ports are drawn from a
configurable range (default 40000–49999).

Per-sandbox state `~/.microkitchen/sandboxes/<name>/state.json`: config path,
resolver/proxy ports, bootstrapped flag, applied normalized config, config
hash. Logs: `~/.microkitchen/logs/<name>/bootstrap.log`,
`~/.microkitchen/logs/broker.log`, `~/.microkitchen/broker/audit.log`.

---

## 6. Bootstrap (`mise bootstrap`)

Once after first boot, and on `microkitchen bootstrap`:

1. `admin.mode(sandbox, Open)`.
2. In guest, streamed to the terminal and `bootstrap.log`:
   ```
   command -v mise || curl -fsSL https://mise.run | [MISE_VERSION=v…] sh
   cd /root && mise bootstrap --yes          # global config; cache dir = the shared volume
   ```
3. `admin.mode(sandbox, Enforce)`; record `bootstrapped=true` in `state.json`
   (authoritative) and on the `microkitchen.bootstrapped` label (best effort:
   microsandbox 0.6.18 cannot update labels of a running sandbox, so the label
   may only change at the next start).

On failure the sandbox stays up in enforce mode with `bootstrapped=false`;
`microkitchen bootstrap` retries. `mise_version = "2026.9.6"` in
`~/.microkitchen/config.toml` pins the installed mise.

---

## 7. Egress broker (implements `egress-broker-design.md`)

One daemon, started on demand by any `microkitchen` command that needs it
(`microkitchen broker start|stop|status` for manual control). Single instance
via a lock file; admin interface on `~/.microkitchen/broker/admin.sock` (mode
0600), newline-delimited JSON.

### 7.1 Registry (design §4)

```
register { name, resolver_port?, proxy_port?, mode } → { resolver_port, proxy_port, username, secret_env, secret }
retire   { name }
mode     { name, open | enforce }
list     → [ { name, ports, mode, pending, denied_all, bindings } ]
```

Per registration the broker binds `127.0.0.1:Rn` (UDP+TCP) and
`127.0.0.1:Pn` (TCP). Which listener a request arrives on is the sandbox
identity. The SOCKS5 username (the sandbox name) is verified against the
listener's owner and the password with a constant-time compare; mismatch fails
the handshake and is logged. `retire` drops listeners, expires that sandbox's
bindings, cancels its queued approvals and tears down its flows.

Ownership answer to design §16.1: microkitchen (the CLI) is the orchestrator.
`up` → ensure broker → `register` → export secret → create/start sandbox.
`down` → stop sandbox → `retire`. The broker never creates sandboxes, but it
holds a `SandboxHandle` per registration for the attributor.

### 7.2 Name observer and binding store (design §5, §6)

Forward every query verbatim to the upstream resolvers (host resolv.conf /
system config; overridable in `~/.microkitchen/config.toml`), return the
response **unmodified**, never block, never synthesize (upstream failures are
forwarded as-is). Record, per sandbox: for A/AAAA answers every name in the
CNAME chain plus the query name; for `HTTPS`/`SVCB` answers the `ipv4hint`/
`ipv6hint` addresses bound to the target and query names. Everything else is
forwarded and ignored.

`BindingStore { (sandbox, ip) → { names: BTreeSet, expires_at, last_seen } }`,
TTL with floor 1 h and cap 24 h, refreshed on every matching connection,
capped per sandbox (default 10 000, LRU eviction; eviction pressure is logged).
Bindings are never shared across sandboxes.

Ambiguity is resolved exactly as design §6.1: empty → prompt as unresolved;
one name → use it; several with agreeing verdicts → apply and log the
ambiguity; several with disagreeing verdicts → prompt showing all candidates.
Reverse DNS is never consulted.

### 7.3 Flow mediator (design §7)

- **Hostname grammar** (`mediator/grammar.rs`): LDH labels 1–63 bytes, total
  ≤253, ASCII only, no leading/trailing hyphen. Applied to SOCKS domain-name
  fields and to rule entries. Reject, never repair.
- **TCP**: greeting requires method 0x02 (user/pass); `CONNECT` builds the
  admission record, awaits the decision engine (unbounded from the client's
  point of view), then dials and splices with `copy_bidirectional` plus explicit
  half-close (shutdown write on EOF per direction). Denied → reply 0x02 and
  close.
- **UDP**: `UDP ASSOCIATE` binds a relay `UdpSocket` on `127.0.0.1:0` and
  advertises that concrete address. The control stream is held open only to
  observe close; close cancels the association's `CancellationToken` (relay,
  upstream sockets, pending approval). Per datagram: parse header, drop
  `FRAG != 0`, validate address type through the same grammar, look up the
  association's verdict cache; on first sight of a destination, request a
  decision and queue up to 16 datagrams while pending, dropping beyond that.
  Upstream replies are accepted only from the exact `(ip, port)` the datagram
  was sent to.
- Admission record: `{ sandbox, transport, address, port, names }`. The
  attribution result lives in a separate struct handed only to the renderer
  (design §8.1, structural).

### 7.4 Attributor (design §8): `mk-whodial` shell script

`scripts/guest/whodial.sh`, POSIX `sh` + `awk` only (both present in
`cruizba/ubuntu-dind`; no `ss`, `lsof`, `python`, or GNU-only flags), embedded
into `microkitchen` with `include_str!` and installed through the SDK's
`script()` mechanism at creation time. The design's argument for a static
binary (images without a shell) does not apply: the image is fixed by the
product spec.

```
mk-whodial <tcp|udp> <ip> <port>  →  {"found":true,"pid":412,"name":"node"}
                                     {"found":false}
```

Algorithm:

1. Convert the destination to the kernel's hex form (`/proc/net/tcp` stores
   IPv4 as little-endian hex, IPv6 as four little-endian 32-bit words; `awk`
   does the byte swapping) and port as 4 uppercase hex digits.
2. Enumerate distinct network namespaces by `readlink /proc/[pid]/ns/net`,
   keeping one representative pid per namespace, so processes inside Docker
   containers are covered.
3. For each namespace read `/proc/<rep>/net/{tcp,tcp6}` or `{udp,udp6}`,
   select rows whose `rem_address` equals the target (TCP: any non-`LISTEN`
   state, `SYN_SENT` expected while the broker holds the reply; UDP:
   connected sockets only, honest "not found" otherwise) and collect their
   inode numbers.
4. Resolve inode → pid by scanning `/proc/[pid]/fd/*` symlinks for
   `socket:[<inode>]`, restricted to pids in the matching namespace.
5. Print `comm` (`/proc/<pid>/comm`) only. Never the command line, never
   arguments (design §8: they carry credentials and the string goes to a dialog
   and an audit log).

The whole script runs under `timeout 1` so the deadline is enforced in the
guest as well as by the broker. The broker invokes it via the SDK
(`handle.connect().exec("mk-whodial", args)`) with a 1 s host-side deadline and
omits the origin line on any failure, timeout, or non-zero exit. It never
blocks the prompt: the request is queued at once and the origin filled in when
the lookup returns; a dialog waits at most the same second for it. Output is
parsed strictly as JSON with the two fields above; anything else is treated as
"not found". Sandboxes created before milestone 5 have no helper and simply
show no origin.

### 7.5 Decision engine and rule store (design §9)

Precedence, first match wins:

1. malformed → deny
2. hard denies (metadata `169.254.169.254`, link-local, multicast, unspecified,
   broadcast) → deny; not promptable
3. sandbox in **open** mode (bootstrap) → allow
4. **deny** rules from the kitchen file, against every candidate name and the
   address
5. **allow** rules from the kitchen file: a name rule matches only if the
   candidate name matches *and* the address is bound to that name for this
   sandbox; address/CIDR rules match the address
6. **session** decisions (per sandbox, in memory): temporary allows with expiry
7. prompt
8. default deny

Coalescing key `(sandbox, name-or-address, port)`; concurrent flows await the
first requester's outcome.

**Rule store = the kitchen file.** Persistent rules are the project's
`[_.microkitchen.network].allow/deny` (product spec). Because one kitchen file
maps to one sandbox, "persistent global across sandboxes" from the design
collapses to "persistent for this project"; a shared `~/.microkitchen/rules.toml`
with the same grammar is consulted after the project rules for operators who
want registry-style global allows (`microkitchen net allow --global`). The
broker watches both files (`notify`) so hand edits and `remodel` apply live.
Store write failures are logged loudly, the current flow still gets its verdict
(design §12).

### 7.6 Approval surface (design §10) and reconciliation with the spec

One global queue, strictly one dialog at a time across sandboxes. Each request
carries the flow's `CancellationToken`; a request whose flow died is dropped
unshown. Dialog content follows design §10 verbatim, including the visually
distinct "this sandbox never resolved this address" line.

**Buttons** (product spec wins on the three options; design semantics fill in
the rest):

| Button | Effect |
|---|---|
| **Deny** | appended to `deny` in the kitchen file (persistent); the flow is refused |
| **Allow** | appended to `allow` in the kitchen file as the exact name, or the address when unresolved (persistent) |
| **Allow 5 min** | session decision for this sandbox, expires after 5 minutes; not persisted |
| dismissed / timeout (default 60 s) | deny this flow only; nothing remembered |

Wildcard scope ("this name and its subdomains") and global scope are available
from the CLI (`microkitchen net allow '*.example.com' [--global]`) rather than
as extra dialog buttons, to keep the dialog to the three specified choices.

Backends, chosen by `approval.dialog = "auto" | "zenity" | "kdialog" |
"osascript" | "none"` (`auto`: osascript on macOS, otherwise zenity or kdialog
when `DISPLAY`/`WAYLAND_DISPLAY` is set): Linux `zenity --question --switch
--extra-button Deny --extra-button "Allow 5 min" --extra-button Allow --timeout
60` (label on stdout; empty = dismissed; exit 5 = timeout), or `kdialog --menu`
(a menu rather than `--yesnocancel`, so closing the window means nothing; no
timeout of its own, the queue closes it at the deadline); macOS `osascript …
display dialog … buttons {…} giving up after 60`. Every request is also exposed
via the admin socket (`microkitchen net pending`, `net decide <id>
allow|deny|temp`), so the CLI can answer while a dialog is up. The fallback when
no dialog can be shown is `approval.headless = "deny" | "queue"` in
`~/.microkitchen/config.toml`, default `deny` (explicit, never inferred).

**Rate limit**: more than `approval.max_prompts` (default 20) prompts within
`approval.window_secs` (default 600) for one sandbox switches it to deny-all
(persisted, so a broker restart does not lift it), notifies once
(`notify-send`/`osascript` notification and the log), and requires `microkitchen
net resume`. Only prompts that reach a human count: with the default headless
`deny` nobody is being flooded.

Audit log (`~/.microkitchen/broker/audit.log`, JSON lines): every verdict with
sandbox, destination, candidate names, source of the decision, ambiguity
flags, and the attribution as a display-only field.

### 7.7 Failure behaviour

As design §12. Notably: broker restart → all registered sandboxes lose egress
until it is back (`microkitchen up`/`status` restart it; persistent rules
survive, bindings and session decisions do not, so first flows re-prompt).
Documented in the README as an inherent property.

---

## 8. CLI

| Command | Behaviour |
|---|---|
| `microkitchen` (= `up`) | discover → validate → resolve env → ensure broker → register → create or start sandbox (by label) → bootstrap if needed → attach shell |
| `up [--no-shell] [--recreate]` | as above without shell; `--recreate` retires, removes, re-registers, recreates (cache volume kept) |
| `shell`, `exec -- <cmd>` | attach / run command |
| `stop`, `start`, `restart` | lifecycle; `start` re-exports the proxy secret first |
| `down [--purge]` | stop + remove sandbox, `retire`; `--purge` also deletes state/logs (never the cache volume) |
| `status`, `list`, `logs [--bootstrap\|--broker\|--sandbox]` | `list` uses labels |
| `bootstrap` | re-run §6 |
| `validate` | §3 + env resolution dry run (values redacted) |
| `remodel [--yes] [--recreate]` | §9 |
| `net pending`, `net decide <id> <allow\|deny\|temp>`, `net allow <rule> [--global]`, `net deny <rule> [--global]`, `net temp <host>`, `net rules`, `net revoke <rule>`, `net mode <open\|enforce>`, `net resume`, `net bindings` | headless approvals, rule review/revocation (design §16.3), rate-limit resume |
| `broker start\|stop\|status\|run` | `run` is the foreground daemon entry (hidden) |

Global: `-C <dir>`, `--home` (`MICROKITCHEN_HOME`, default `~/.microkitchen`),
`-v/-q`, `--json` where sensible.

---

## 9. `microkitchen remodel`

1. Load + validate config, resolve env, locate sandbox by label; load the
   **applied** normalized config from `state.json`.
2. Structured diff (field, before, after), classified:

   | Change | Class | How |
   |---|---|---|
   | `cpus`, `memory` (≤ ceilings) | Live | `modify().cpus()/.memory().apply()` |
   | env add/change/remove | Future execs | `modify().env()/.remove_env()`; noted as "new commands only" |
   | secret value rotation | Live | `modify().secret(...)` |
   | new secret / secret hosts changed / secret removed | Restart | `modify().secret(...).next_start()`, advise `microkitchen restart` |
   | `disk` | Restart | `modify().root_disk_size().next_start()` |
   | network `allow`/`deny` | Live | broker reloads the kitchen file |
   | `ports`, `mounts`, `network` preset, image | Recreate | advise or perform `--recreate` |

   The SDK's `modify().dry_run()` plan (dispositions, warnings) is shown next to
   ours, and it decides what is live: in microsandbox 0.6.18 CPU and memory
   are live only when the runtime's control socket can resize and the target
   fits the booted capacity, environment changes always need a restart on a
   running sandbox, and so do added secrets. Groups the dry run marks
   "requires restart" are applied with `next_start()` and the user is told to
   run `microkitchen restart`; the rest apply immediately.

   Secret values are compared against the SDK's stored copies (any value sent
   counts as a rotation), host changes against the recorded config; the
   environment is diffed against the sandbox's stored env. `state.json` keeps
   the applied kitchen text for the text diff, and the guest's `mise.toml` is
   rewritten (or, for a stopped sandbox, at its next start).
3. Print the TOML text diff (`similar`) and the classification table; confirm
   unless `--yes`.
4. Apply live changes (`apply()`), persist restart-required ones
   (`next_start()`) with a message naming the command to run, and print or
   perform recreation. Update labels and `state.json`.

---

## 10. Tests

Conventions copied from microsandbox: `#[mk_test]` (`#[tokio::test] #[ignore]`
+ `test_utils::init_isolated_home()` which, under `MK_TEST_ISOLATE_HOME`, points
`MSB_HOME` and `MICROKITCHEN_HOME` at temp dirs and reuses the installed `msb`
via `MSB_PATH`). Run with
`MK_TEST_ISOLATE_HOME=1 cargo nextest run --run-ignored=only --test-threads 2`
(`just test-integration`); plain `cargo test` runs unit tests only. This machine
has KVM and `msb doctor` passes. Integration tests set the `headless` approval
backend and drive the broker through the admin socket via a `BrokerClient`
in `test-utils`; a `TestKitchen` fixture writes a temp project and cleans up
sandbox, registration and files on drop.

**Deterministic, no VM** (design §15 first block, plus microkitchen's own):

- binding store: CNAME chains bind every name; `HTTPS`/`SVCB` hints bind;
  per-sandbox isolation; TTL floor/cap; LRU eviction under cap
- ambiguity: every row of design §6.1
- precedence: allow-by-name does **not** match when the address is not bound
  to that name for that sandbox; deny beats allow; open mode; hard denies not
  promptable
- grammar: null bytes, newlines, oversized labels, non-ASCII → rejected, never
  repaired (SOCKS fields and rule entries)
- SOCKS5 codec: greeting/auth, CONNECT replies, UDP header round-trip,
  fragments dropped, malformed headers rejected
- association lifetime: closing the control stream cancels relay, upstream
  sockets and the pending approval
- relay filtering: datagram from an endpoint never sent to is dropped
- approval queue: strict serialization; dead-flow request dropped unshown;
  rate limit trips at threshold; timeout/dismiss not persisted
- **attribution cannot influence a verdict**: identical admission records with
  and without an origin produce identical decisions (type-level: the decision
  function does not take the field)
- config: size/pattern parsing, schema and validation errors, `[_.microkitchen]`
  vs legacy handling, `toml_edit` round-trips preserving comments, remodel
  classification, env merge (pass-through omission, empty = absent)
- `whodial.sh`: Rust unit tests in `broker::attribution` build fake `/proc`
  trees and run the script through `sh` with `PROC_ROOT` overridden (hex
  address parsing, IPv6 word swapping, IPv4-mapped sockets, multiple netns,
  inode → pid within the namespace, `LISTEN` excluded, UDP unconnected → not
  found, malformed rows and arguments, name sanitizing), so `cargo test`
  covers it; `just test-scripts` runs just those

**Integration** (`crates/microkitchen/tests/`):

| File | Covers |
|---|---|
| `config_discovery.rs` | walks up; kitchen file selection; mise precedence |
| `env_and_secrets.rs` | plain env visible; secret is a placeholder; placeholder passes through to a non-allowed host (host HTTP fixture); missing `[env]` declaration is an error; host-only required var forwarded; `{ default = "" }` unset → absent and secret skipped, set → present |
| `docker_in_sandbox.rs` | `docker info`; `docker run --rm hello-world` (open mode) |
| `labels.rs` | found by `microkitchen.config` label; `list`; second `up` reuses |
| `mise_cache_volume.rs` | cache volume populated; second kitchen sees it |
| `bootstrap.rs` | no pending approvals during bootstrap (open mode); tool from `[tools]` usable; enforce afterwards |
| `network_policy.rs` | allowed name reachable; denied name refused; unknown name → pending approval shows the name; `decide allow` completes the connection and appends to `mise.toml`; `decide deny` appends to `deny`; `decide temp` not persisted and expires (`MICROKITCHEN_TEMP_ALLOW_SECS` override) |
| `unresolved_address.rs` | hard-coded public IP over TCP → pending approval flagged "never resolved"; allow writes the address |
| `laundering.rs` | allow rule for name A; guest connects to an address bound only to name B on the same IP set → prompts, does not inherit A |
| `udp_policy.rs` | UDP datagram to unlisted public IP → pending with `transport=udp`; deny leaves no pending; QUIC-style flow after an `HTTPS` record lookup shows the name (binding from hints) |
| `resolver_bypass.rs` | guest `dig @1.1.1.1` and DoT to port 853 are refused by microsandbox policy |
| `process_attribution.rs` | `curl` to unknown host → pending shows `name=curl`, pid; same from inside a Docker container |
| `two_sandboxes.rs` | two kitchens registered concurrently; approvals serialized; bindings isolated; retiring one leaves the other working |
| `broker_restart.rs` | kill broker under load → flows fail closed; `up` restarts it; persistent rules survive; first flow re-prompts |
| `rate_limit.rs` | burst of unresolved addresses trips deny-all; `net resume` restores |
| `ports.rs` | published TCP port reachable from host |
| `remodel.rs` | cpus/memory/env live; disk → restart message and `next_start`; mounts → recreate message |

---

## 11. Milestones

1. **Scaffold + config** — workspace, CLI skeleton, discovery, schema,
   validation, size/grammar/pattern parsing, `toml_edit` writer, mise env
   resolution + merge rules, `validate`. Unit tests.
2. **Sandbox lifecycle (no broker)** — naming/labels, builder mapping, cache
   volume, dind init script, ports, mounts, env/secrets, `up/stop/start/down/
   status/list/shell/exec`; sandboxes temporarily created without proxy.
   Spike first: dockerd inside the ubuntu-dind microVM. Tests: labels,
   env_and_secrets, docker_in_sandbox, ports.
3. **Bootstrap** — guest config rendering, mise install, runner, logs. Tests:
   bootstrap, mise_cache_volume.
4. **Broker core** — daemon/admin socket/registry, observer + binding store,
   SOCKS5 mediator (TCP, then UDP), decision engine + rule store + file watch,
   headless approval backend, audit log. Wire proxy/DNS/structural denies into
   creation. Tests: network_policy, unresolved_address, laundering,
   udp_policy, resolver_bypass, two_sandboxes.
5. **Approval surface + attributor** — dialog backends, queue, rate limit,
   `whodial.sh` with fixture tests, staging via `script()`. Tests:
   process_attribution, rate_limit, broker_restart.
6. **remodel** — diff, classification, apply/next_start/recreate. Test:
   remodel.
7. **Polish** — README (config reference, mise caveats, dialog dependencies,
   broker availability coupling), `justfile`, GitHub workflow (unit tests on
   hosted runners; integration job for a KVM runner), error-message pass.

---

## 12. Risks and open points

- **dockerd in the ubuntu-dind microVM**: verified in milestone 2 (flat ext4
  root, systemd via `.init("auto")`, overlayfs storage driver, cgroup v2;
  `docker run hello-world` works, also after stop/start).
- **Proxy secret at start**: microsandbox reads the password from the host
  environment at every start, so `msb start <name>` outside microkitchen will
  fail auth. Documented; `microkitchen start` is the supported path.
- **Immutable endpoints**: handled by persisted ports + `register` with
  requested ports; conflicts need `--recreate`.
- **Broker availability coupling**: when the broker is down every kitchen has
  no egress. Stated in docs (design §12); `up`/`status` restart it.
- **DNS over HTTPS** cannot be closed at the port layer; such flows surface as
  "never resolved" addresses (design §11.1). The kitchen `allow` list is the
  real control.
- **UDP attribution** only works for connected sockets; QUIC clients typically
  connect, NTP-style clients often do not. Reported honestly as "not found".
- **Helper script portability** is tied to the fixed image: it relies on
  `sh`, `awk`, `readlink`, and `timeout` from Ubuntu. If the base image ever
  changes, the fixture tests catch missing tools early, and the broker already
  degrades to "no attribution" on any failure.
- **Secret values at rest** live in microsandbox's local store; `down --purge`
  removes them with the sandbox.
- **Rate-limit thresholds** (design §16.2) start at 20/10 min and are logged so
  they can be tuned after observing a real dependency install.
- **Spec example `required = false`** is invalid in mise; `{ default = "" }` is
  the supported form. README documents this and the `[_.microkitchen]` name.
