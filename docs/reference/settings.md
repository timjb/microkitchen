# Settings

`~/.microkitchen/config.toml` (every key optional):

```toml
mise_version = "2026.9.6"      # mise installed in guests (default: latest)

[approval]
dialog = "auto"                # auto | zenity | kdialog | osascript | none
headless = "deny"              # no dialog: "deny" or "queue" for `net decide`
timeout_secs = 60              # unanswered approvals deny the connection
max_prompts = 20               # more prompts than this within window_secs
window_secs = 600              #   switch a sandbox to deny-all

[broker]
port_range = [40000, 49999]    # per-sandbox resolver and proxy ports
upstream_dns = ["1.1.1.1"]     # default: the host's /etc/resolv.conf
```

`dialog = "auto"` uses `osascript` on macOS and, when `DISPLAY` or
`WAYLAND_DISPLAY` is set, `zenity` or `kdialog` on Linux. For `headless`, see
[Headless use](../guide/network#headless-use).
