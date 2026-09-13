# Egress Broker — interactive network mediation for microsandbox VMs

A host-side daemon that mediates all outbound TCP and UDP from a set of
microsandbox microVMs. It recovers the hostnames the sandbox's own DNS
forwarder would otherwise hide, attributes each flow to the guest process
that opened it, and asks a human for a decision through a native desktop
dialog.

This document describes *what* the system is and *why* each part exists. It
is implementation-language neutral; section 14 states what a candidate
language must provide.

**Decided up front** (these close off the main forks; see §16 for what
remains open):

- One daemon serving many sandboxes.
- A small helper binary staged into each guest at boot, invoked on demand.
- A native desktop dialog on the host as the approval surface.

---

## 1. Problem

A coding agent runs inside a microVM. It can install packages, clone
repositories, call APIs. It can also exfiltrate a repository, phone home to
a hard-coded address, or pull a dependency nobody vetted. A static allowlist
is either too narrow to let real work happen or too broad to mean anything.

The broker's job is to make each *new* destination a decision a human makes
once, with enough context to decide well: which sandbox, which process, and
above all *what name* the destination corresponds to. After the decision, it
gets out of the way.

---

## 2. Environment constraints

These come from how microsandbox works and are not negotiable. Every design
choice below traces back to one of them.

| Constraint | Consequence |
| --- | --- |
| All guest traffic terminates in a host-side user-space network stack; there is no kernel routing or NAT in the path. The flow that reaches an external proxy is a *fresh* connection originated by microsandbox. | The broker cannot learn anything about the guest from the connection itself — not the process, not the original source port. Everything interesting must be recovered out of band. |
| The outbound proxy is SOCKS5: `CONNECT` for TCP, `UDP ASSOCIATE` for non-DNS UDP. One proxy per sandbox, local only. | The broker's mediation interface is fixed. It must implement both commands. |
| DNS never traverses the proxy. microsandbox resolves on the host through its own forwarder — plain DNS, DNS-over-TCP, and DNS-over-TLS alike. | The broker sees only IP addresses on the proxy path. Hostnames must come from a second channel. |
| Nameservers are configurable per sandbox, as address or hostname, with optional port. | That second channel: the broker *is* the sandbox's resolver. |
| microsandbox evaluates its own network policy against the real destination *before* opening the proxy connection. | microsandbox is the outer layer. The broker only ever sees what microsandbox already permitted. |
| Each TCP connection gets its own proxy connection and handshake; each UDP flow gets its own control connection and association. | Per-flow state is natural. An association is effectively one destination. |
| Traffic to `host.microsandbox.internal` bypasses the proxy. | Invisible to the broker. Must remain denied by microsandbox policy. |
| Proxy URIs reject embedded credentials; SOCKS5 username and password are configured separately, the password sourced from a host environment variable read once at sandbox start. | The broker can mint a per-sandbox secret, but it must exist in the environment before the sandbox starts. |
| microsandbox rewrites private, loopback, and link-local DNS answers to `NXDOMAIN` unless an explicit address rule permits them, and refuses DoQ, mDNS, LLMNR, and NetBIOS-NS so guests fall back to ordinary DNS. | Rebinding protection is already handled upstream. The broker does not duplicate it, and the refused transports narrow the bypass surface. |
| DNS-over-HTTPS on port 443 is indistinguishable from ordinary HTTPS. | The name channel is bypassable by a determined guest. Handled by policy, not by cleverness (§10). |

---

## 3. System overview

```
  guest VM                          host
 ┌──────────────┐
 │ agent, npm,  │
 │ curl, python │
 │   helper ◄───┼──── exec, on demand ─────────────┐
 └──────┬───────┘                                  │
        │ virtio-net                               │
 ┌──────▼────────────────────────┐                 │
 │ microsandbox                  │                 │
 │  ├ DNS forwarder ──────┐      │                 │
 │  ├ network policy      │      │                 │
 │  └ SOCKS5 client ──┐   │      │                 │
 └────────────────────┼───┼──────┘                 │
                      │   │                        │
 ┌────────────────────▼───▼────────────────────────┼──────┐
 │ EGRESS BROKER (one daemon)                      │      │
 │                                                 │      │
 │  ┌─ admin interface ── register / retire sandboxes      │
 │  │                                              │      │
 │  ├─ per-sandbox resolver endpoints ─► NAME OBSERVER     │
 │  │                                        │     │      │
 │  │                                   BINDING STORE      │
 │  │                                        │     │      │
 │  ├─ per-sandbox proxy endpoints ─► FLOW MEDIATOR        │
 │  │                                        │     │      │
 │  │                                  DECISION ENGINE     │
 │  │                                    │         │      │
 │  │                        ┌───────────┴──────┐  │      │
 │  │                        │                  │  │      │
 │  │                 DECISION STORE      ATTRIBUTOR ──────┘
 │  │                                           │
 │  └────────────────────────────────► APPROVAL QUEUE
 │                                             │
 │                                      desktop dialog
 └──────────────────────────────────────────────────────────┘
```

Seven components. Each is described below with its inputs, its outputs, and
the invariant it must hold.

---

## 4. Sandbox registry

**Purpose.** Give every sandbox a stable identity and the two loopback
endpoints it will be pointed at.

The broker is a daemon, so it must learn about sandboxes before they start.
It exposes an **admin interface** on a local IPC channel (Unix domain socket
or named pipe, filesystem permissions as the access control) with three
operations:

```
register(sandbox_name, options) -> { resolver_endpoint,
                                     proxy_endpoint,
                                     proxy_username,
                                     proxy_secret }
retire(sandbox_name)
list() -> [sandbox_state]
```

Whoever creates sandboxes calls `register` first, exports the secret into
the environment variable microsandbox will read, creates the sandbox with
the returned endpoints, and calls `retire` when it is gone.

**Identity is the endpoint.** The broker binds a distinct loopback
address/port pair per sandbox for the resolver and for the proxy. Which
endpoint a request arrived on *is* the sandbox identity.

This is the most important piece of plumbing in the system, and it is worth
being explicit about why nothing cheaper works. DNS queries arrive from
microsandbox's host-side forwarder, not from the guest, so the query source
tells you nothing. Proxy connections arrive from the microsandbox process
for the same reason. Without per-sandbox endpoints, every request from every
sandbox is indistinguishable, and the binding store — which must be
per-sandbox to be sound (§6) — cannot be keyed at all.

The SOCKS5 username is a secondary check: microsandbox transmits it
verbatim, so the broker verifies it matches the sandbox that endpoint was
allocated to. A mismatch means a configuration error or a misrouted
sandbox; fail the handshake loudly rather than guessing.

**Retirement** drops the endpoints, expires that sandbox's bindings, and
cancels any pending approvals for it.

---

## 5. Name observer

**Purpose.** Learn every hostname each sandbox resolves, and what addresses
it was told to use.

Each sandbox's resolver endpoint accepts DNS over both UDP and TCP. For
every query the observer forwards it to the configured upstream resolvers,
parses the response, records what it learned, and returns the response
**unmodified**.

### 5.1 The observer never blocks and never filters

This is the design's least obvious decision, so it is worth stating the
reasoning.

The intuitive design prompts here: a new name appears, ask the human, answer
or refuse accordingly. It does not survive the timeouts. microsandbox moves
to the next configured resolver on timeout or connection failure, and the
guest's own stub resolver gives up after a few seconds. A human reading a
dialog is slower than both. Stalling a DNS answer produces resolver
failover, guest-side retries, duplicate queries, and a dialog racing a
request that has already been abandoned — and it produces them
*intermittently*, which is worse than producing them always.

The proxy path has no such constraint: a SOCKS5 client blocks indefinitely
on the reply to `CONNECT`. That is the only point in the flow where an
unbounded human delay is safe, so that is where approval happens.

The observer therefore has no policy role whatsoever. It observes. This also
means a resolver failure degrades to "no name known" rather than to "no
network".

### 5.2 What gets recorded

For each answer, the observer records a binding for **every name in the
chain**, not just the queried name. A query for `api.example.com` that
CNAMEs through `example.map.cdn.net` before yielding an address must bind
both, or the dialog will later show the operator a CDN name they have never
seen instead of the name their code used.

It also records addresses from `HTTPS`/`SVCB` answers (`ipv4hint`,
`ipv6hint`). Modern clients connect to those without a separate address
lookup; miss them and ordinary HTTPS traffic shows up as unexplained
hard-coded addresses.

Everything else — `MX`, `TXT`, `PTR` — is forwarded and ignored.

---

## 6. Binding store

**Purpose.** Answer the question the flow mediator actually needs: *given
this sandbox and this destination address, what names might this be?*

```
binding {
  sandbox     : sandbox identity
  address     : IP address
  names       : set of names (query name + CNAME chain)
  expires_at  : timestamp
  last_seen   : timestamp
}
```

Three properties matter.

**Per-sandbox.** Bindings are never shared across sandboxes. If they were,
one sandbox's lookup would launder another sandbox's connection: sandbox A
resolves an allowed name, sandbox B connects to the resulting address and
inherits the name. Keying by sandbox costs nothing and closes it.

**Generously expiring.** Guests cache DNS, and some runtimes pin a resolved
address for the process lifetime, so a connection can legitimately arrive
long after the TTL lapsed. Expiry here bounds memory; it is not a security
control — microsandbox already enforces rebinding protection upstream.
Use the answer's TTL with a substantial floor (order of an hour) and a cap
(order of a day), and refresh on every matching connection.

**Bounded.** Cap entries per sandbox and evict the least recently seen. A
guest resolving an unbounded number of names is itself worth logging.

### 6.1 Ambiguity

One address serves many names, and the reverse map is inherently
many-valued. The store returns a *set*; the decision engine resolves it:

| Candidate set | Resolution |
| --- | --- |
| Empty | Treat as an unresolved address. Prompt showing the raw address, stating plainly that this sandbox never looked it up. |
| Exactly one name | Use it. |
| Several, stored verdicts agree | Apply that verdict; record the ambiguity in the audit log. |
| Several, stored verdicts disagree | Prompt, showing every candidate and its verdict. |

Neither shortcut is acceptable. "Deny if any candidate is denied" means one
blocked name on a shared CDN address takes down every other name on it.
"Allow if any candidate is allowed" is a direct bypass: resolve something
allowed, connect to something denied on the same address. Disagreement is
rare in practice, so prompting costs little and is the only sound answer.

Reverse DNS is never used to disambiguate — it is controlled by whoever owns
the address, which is precisely the party in question.

---

## 7. Flow mediator

**Purpose.** Terminate the SOCKS5 protocol, gather context, ask the decision
engine, and either carry the traffic or refuse it.

### 7.1 Common admission record

Both transports produce the same record before any decision is made:

```
admission_request {
  sandbox      : sandbox identity          (from endpoint)
  transport    : tcp | udp
  address      : destination IP
  port         : destination port
  names        : candidate names           (from binding store)
  origin       : process description       (from attributor, may be absent)
}
```

Note that `address` is always present and `names` may be empty — the
opposite of a conventional proxy, and a direct consequence of microsandbox
resolving before we ever see the flow.

### 7.2 TCP

Standard SOCKS5 `CONNECT`. Authenticate with the per-sandbox username and
secret. microsandbox always sends an address literal since it has already
resolved, but the domain-name address type must still be accepted and
validated for completeness.

**Validate, never repair.** The domain-name field in SOCKS5 is a
length-prefixed byte string: a client can put null bytes, control
characters, or arbitrary non-ASCII in it. Names that do not match the
hostname grammar are rejected outright. They are never trimmed, truncated at
a null, or otherwise normalized into something acceptable — a repair step is
exactly where the decision engine and the connection layer end up judging
different strings, which is a well-documented source of allowlist bypasses.

On approval, connect to the destination and splice both directions with
proper half-close handling, so one side finishing does not truncate the
other's response. On refusal, reply with the "not allowed by ruleset" code
and close.

### 7.3 UDP

`UDP ASSOCIATE` opens a control connection and a relay socket:

- The **control connection is the association's lifetime.** It is held open
  and monitored for close, but never carries data. Its termination tears
  down the relay socket, the upstream sockets, and any pending approval.
- The reply must advertise a concretely reachable relay address, never a
  wildcard.
- Each datagram carries a header with a fragment field, an address type, and
  the destination. **Fragmented datagrams are dropped, not reassembled.**
  Nothing in practice uses fragmentation, and a reassembly buffer keyed on
  attacker-chosen identifiers is a liability with no upside.
- Address-type handling, including the domain-name grammar, is identical to
  the TCP path. Same reasoning, same code path if possible.
- **Replies are accepted only from the exact endpoint a datagram was sent
  to.** The relay socket is reachable by anything on loopback; an
  unrestricted relay is an open reflector.

**Approval and loss.** The first datagram to a new destination triggers
approval for the association. Datagrams arriving while approval is pending
are queued to a small bound and then dropped. UDP is lossy by contract and
clients retransmit; an unbounded queue is a memory attack and a stalled
sender is worse than a dropped packet. Since microsandbox opens a separate
association per flow, an association is usually a single destination and the
verdict cache holds one entry.

**What this path carries.** Not DNS — that never reaches the proxy. In
practice: QUIC/HTTP3, NTP, WebRTC. For QUIC the binding store is the *only*
source of a name, since microsandbox does not intercept QUIC and the name is
not otherwise visible. That makes §6 load-bearing rather than convenient.

---

## 8. Attributor

**Purpose.** Tell the operator which process inside the guest opened the
flow.

**Mechanism.** A small static helper binary is staged into each guest at
bootstrap. When the decision engine needs a prompt, the broker executes the
helper in that sandbox through microsandbox's command execution channel,
passing the destination address, port, and transport. The helper returns a
structured record.

**Why this works.** Because the path contains no port-rewriting NAT, the
destination the guest asked for is identical to the destination the broker
was asked for. So the helper can search the guest's own kernel socket tables
for a socket whose *remote* endpoint matches, then map that socket to the
process holding it. And because the guest socket sits in a connecting state
while the broker holds the SOCKS reply, it is reliably present for the whole
lifetime of the dialog.

**Helper contract.**

```
input   : transport, destination address, destination port
output  : { found: bool, pid: int, name: string }
```

The helper reports the process's short name only, never its full command
line: arguments routinely carry credentials, and this string is rendered in
a dialog and written to an audit log.

**Why a staged binary rather than a shell one-liner.** Many images have no
shell and no standard text utilities. A single static binary gives a stable
output contract, works on a scratch image, and keeps the parsing on the side
that can be tested.

**Bounds.** The lookup runs with a short deadline (order of one second) and
never blocks the prompt. If it fails, times out, or the sandbox has no
helper, the dialog simply omits the origin line.

**Known gaps, stated rather than papered over.** Unconnected UDP sockets
have no remote endpoint recorded and will usually not attribute. Very
short-lived processes can exit before the lookup runs. Both return "not
found", which is honest.

### 8.1 The invariant

**Attribution is display-only. It never enters the decision engine.**

The data originates inside an untrusted guest. A compromised guest can
present whatever process table it likes. Feeding that into a verdict would
let the thing being judged write its own evidence. The operator sees it,
weighs it, and decides; the machine does not.

This must be structural, not a convention — the decision engine should not
receive the field at all, or should receive it through a channel only the
renderer reads.

---

## 9. Decision engine

**Purpose.** Turn an admission request into allow or deny, consulting stored
rules first and a human only when nothing stored applies.

### 9.1 Precedence

First match wins:

1. **Malformed request** — a domain name failing the grammar check. Deny.
2. **Hard denies** — cloud metadata addresses, link-local, multicast,
   unspecified. Not promptable and not unlockable by any rule or answer.
   microsandbox blocks these too; the duplication is deliberate and free.
3. **Deny rules** — evaluated against every candidate name *and* the
   address.
4. **Allow rules** — require **both** that a candidate name matches the rule
   *and* that the destination address is bound to that name for this
   sandbox. A name rule matching on the name alone would be satisfiable by
   anything that claims the name.
5. **Session decisions**, then **persistent decisions**, keyed by name where
   one is known and by address otherwise.
6. **Prompt.**
7. **Default** — deny.

Rule 4 is the one to get right. It is the same requirement microsandbox
applies internally to its own domain rules, and it is what makes a
name-based allowlist meaningful in a system where the connection carries
only an address.

### 9.2 Decision scopes

An approval answers for one of:

- **Once** — this flow only. Never persisted.
- **Session** — until the sandbox retires. Held in memory, per sandbox.
- **This name** — persisted, applies to every sandbox.
- **This name and its subdomains** — persisted, applies to every sandbox.
- **This address** — persisted; offered only when no name is known. No
  wildcard variant, because there is no meaningful hierarchy over addresses.

Persistent rules are **global across sandboxes** and session decisions are
**per sandbox**. The rationale: "I trust this package registry" is a fact
about the world that an operator should not have to restate for each new
worker, whereas "just this once, for this job" is by definition scoped to
the job.

A **dismissed dialog denies once and is never remembered.** Closing a window
is not a policy statement. Likewise a **timeout denies and is not
remembered** — silence is not consent, and a remembered non-answer would
quietly harden into a rule nobody made.

### 9.3 Coalescing

Concurrent flows to the same destination share one approval. The key is
`(sandbox, name-or-address, port)`; the first requester prompts and the rest
wait on its outcome. Without this, a parallel dependency install against one
CDN address produces a dialog per connection.

---

## 10. Approval surface

One host, one human, one screen. The desktop dialog is therefore a
**serialized, global resource**, and the approval queue is a real component
rather than a function call.

- **Strictly one dialog at a time**, across all sandboxes. A second request
  waits in the queue.
- Each queued request carries its own deadline. A request whose flow is
  already gone — control connection closed, client hung up — is dropped from
  the queue without ever being shown. Showing the operator a question about
  a connection that no longer exists trains them to click without reading.
- The dialog identifies **which sandbox** is asking, since the operator may
  have several running.

**Content, in priority order:**

```
Sandbox:      worker-3
Destination:  140.82.121.3 : 443  (TCP)
Resolved as:  api.example.com
              also seen as: example.map.cdn.net      ← only if ambiguous
Process:      pid 412 (node)                         ← only if attributed
```

The single most valuable line the dialog can show is the *absence* of a
name: "this sandbox never resolved this address" is the exact shape of a
hard-coded command-and-control address or an exfiltration attempt, and it
should be visually distinct from the ordinary case rather than an empty
field.

**Rate limiting is mandatory, not a refinement.** A guest can trigger
unbounded dialogs by connecting to many unresolved addresses. After N
approvals requested within a window for one sandbox, the broker switches
that sandbox to deny-all, notifies once, and requires an explicit operator
action to resume. Without this, the approval surface is a denial-of-service
target and, worse, a click-fatigue attack: bury one real request in two
hundred noise requests.

**Headless behaviour.** When no dialog can be shown, the configured fallback
applies, defaulting to deny. This must be an explicit configuration choice,
never an inferred one.

---

## 11. Policy layering and configuration

microsandbox evaluates its policy first. The two layers should be given
different jobs:

- **microsandbox owns the structural rules** — the things that are never up
  for discussion and should not generate a dialog: no private ranges, no
  loopback, no link-local, no cloud metadata, no host access. These are
  already its defaults.
- **The broker owns the interactive, per-name decisions** — which public
  destinations this workload may reach.

Consequently sandboxes are created with egress permitted broadly at the
microsandbox layer *except* for the structural denies, so that requests
reach the broker rather than being refused before any dialog can appear. If
microsandbox's own egress remains deny-by-default, the broker is decorative:
it will only ever be asked about traffic microsandbox already allowed.

### 11.1 Closing the name channel's bypasses

The binding store only knows what the observer sees. Three leaks, two
closable:

- **A guest targeting its own resolver directly** — closed by denying egress
  to DNS ports for everything except the sandbox gateway.
- **DNS over TLS** — closed the same way, by denying its port.
- **DNS over HTTPS** — not closable at the port layer, since it is
  indistinguishable from ordinary HTTPS. Only an egress allowlist closes it.

The third is a genuine limitation. Its consequence is not a silent bypass,
though: a guest that hides its name resolution connects to an address with
no binding, and the dialog reports exactly that. The system degrades to
"unexplained address" rather than to "wrong name".

---

## 12. Failure behaviour

| Failure | Behaviour |
| --- | --- |
| Upstream DNS unreachable | Forward the failure verbatim. Never synthesize an answer — a fabricated `NXDOMAIN` is indistinguishable from a policy denial and costs hours of debugging. |
| No binding for an address | Not an error. Prompt as an unresolved address, flagged. |
| Attribution fails or times out | Omit the line. Never delay or block the prompt. |
| No dialog available | Configured fallback, default deny. |
| Approval times out | Deny this flow. Do not persist. |
| Persistent store write fails | Log loudly, apply the decision to the current flow, keep serving. Remembering is best-effort; mediating is not. |
| Approval rate limit tripped | Sandbox switches to deny-all, one notification, manual resume. |
| Broker crashes or restarts | Every registered sandbox loses egress until it returns. Persistent rules survive; session decisions and bindings do not, so the first flows after a restart re-prompt. This availability coupling is inherent to being in the path and should be stated in operational docs rather than engineered around. |
| Sandbox retired with flows open | Tear down its flows, drop its queued approvals, expire its bindings. |

---

## 13. Threat model

**In scope.** A workload inside a guest reaching a destination its operator
would not approve — by name, by hard-coded address, over TCP or UDP —
including attempts to launder a denied name through an allowed name's
address, and attempts to exhaust or fatigue the operator's attention.

**Explicitly out of scope:**

- **A guest that avoids observable DNS.** Mitigated to "unexplained
  address", not eliminated. An egress allowlist is the real answer.
- **A guest that lies about its processes.** Attribution is advisory by
  construction (§8.1).
- **Payload content.** The broker decides on endpoints. An approved
  connection to an approved host can carry anything, including the entire
  repository. Content-level policy needs TLS interception, which is
  microsandbox's feature, not this one.
- **Flows that bypass the proxy**, notably `host.microsandbox.internal`.
  These must stay denied at the microsandbox layer.
- **Timing and volume side channels** over an approved connection.
- **The host.** The broker is trusted code. Its persistent rules are a
  security-relevant document: restrictive permissions, atomic replacement,
  and write failures treated as operational alerts.

---

## 14. Implementation requirements

Language-neutral, but a candidate must offer:

- **Concurrency with cancellation.** Many simultaneous flows, each of which
  may park on a human for minutes. Whatever the model — threads, tasks,
  fibers — cancellation must propagate cleanly, since a closed control
  connection has to abort a queued approval.
- **Raw TCP and UDP sockets**, with half-close on TCP and per-datagram
  source addresses on UDP.
- **DNS message parsing** sufficient for question, answer, CNAME chains, and
  service-binding hints. A parsing library is fine; a full resolver is not
  needed, since upstream resolution is delegated.
- **Subprocess execution** for the dialog backends and for invoking the
  guest helper through the sandbox runtime's client.
- **Atomic file replacement** for the persistent rule store.
- **Constant-time comparison** for the per-sandbox proxy secret.
- **Cross-compilation to a static binary** for the guest helper, which must
  run on minimal images with no shell, no libc guarantees, and possibly a
  different architecture than the host.

Two distinct binaries ship: the broker and the guest helper. They share only
the helper's output schema, so they need not be written in the same
language, though it is convenient if they are.

---

## 15. Test plan

**Deterministic, no network:**

- Binding store: CNAME chains bind every name; service-binding hints bind;
  per-sandbox isolation; expiry with floor; eviction under cap.
- Ambiguity: each row of §6.1 as a case.
- Precedence: in particular, an allow rule for a name must **not** match when
  the address was never bound to that name for that sandbox.
- Grammar: null bytes, embedded newlines, oversized labels, non-ASCII — all
  rejected, none repaired.
- UDP datagram codec: fragments dropped, malformed headers rejected, reply
  wrapping round-trips.
- Association lifetime: closing the control connection tears down relay,
  upstream sockets, and pending approval.
- Relay filtering: a datagram from an endpoint never sent to is dropped.
- Approval queue: strict serialization; a request whose flow died is dropped
  unshown; rate limit trips at the configured threshold.
- **Attribution cannot influence a verdict** — identical inputs with and
  without an origin value must produce identical decisions.

**Integration, against a real sandbox:**

- Named destination over TCP — dialog shows the name.
- Hard-coded address over TCP — dialog shows "never resolved".
- QUIC destination over UDP — name recovered from the binding store.
- Guest targeting its own resolver — denied by microsandbox policy.
- Attribution end-to-end: start a known process in the guest, connect,
  assert the dialog names it.
- Broker restart under load: flows fail closed, persistent rules survive.

---

## 16. Open questions

1. **Sandbox registration ownership.** The admin interface assumes something
   creates sandboxes and calls the broker first. If that orchestrator does
   not exist yet, the broker may need to wrap sandbox creation itself, which
   turns it from a network component into a lifecycle component.
2. **Rate limit thresholds.** The mechanism is mandatory; the numbers need
   real traffic. Start conservative and log what a normal dependency install
   actually costs in approvals.
3. **Rule review.** Persistent global rules accumulate. There should be some
   way to list, audit, and revoke them — probably part of the admin
   interface, but its shape depends on question 1.
4. **Multi-operator.** The design assumes one human at one screen. A shared
   host with several operators needs a different approval surface entirely.

---

## Sources

microsandbox documentation, retrieved 2026-09-13:
`/networking/outbound-proxy`, `/networking/dns`, `/networking/tls`,
`/security/network`.
