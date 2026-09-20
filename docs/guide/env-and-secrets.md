# Environment variables and secrets

`[env]` is resolved **on the host** with mise (`_.file`, `_.source`, templates
and all). Each resolved variable goes into the sandbox:

- as a **plain environment variable**, unless
- a `[_.microkitchen.secrets.NAME]` table exists for it: then it is a
  microsandbox **secret**. The guest sees only a placeholder; the real value is
  substituted into TLS connections to the secret's `allow` hosts, and the
  placeholder passes through unchanged everywhere else.

```toml
[env]
GITHUB_TOKEN = { required = true }

[_.microkitchen.secrets.GITHUB_TOKEN]
allow = ["github.com", "*.github.com"]
```

Every secret table needs a matching `[env]` declaration. Variables that resolve
to an empty string are not injected; a secret declared for one is skipped with a
notice. The guest's copy of `mise.toml` drops `_.file`, `_.source` and `_.path`,
so `.env` files never enter the sandbox.

## Secrets in files

A [templated dotfile](./dotfiles#secrets-in-templates) renders in the sandbox,
where a secret is only ever the placeholder — so the rendered file holds the
placeholder and the real value never touches the sandbox's disk.

Substitution is a literal match on the bytes sent, so it works only where the
value travels verbatim:

| Works | Does not |
|---|---|
| `Authorization: Bearer <token>` | `~/.netrc` read by curl, which sends `Authorization: Basic base64(user:token)` |
| `git config http.extraheader` | anything that hashes, base64s or URL-escapes the value |

Staging a real credential file as a plain (non-template) dotfile copies it into
the sandbox in clear text, outside this machinery entirely.
`microkitchen validate` lists every host file that will be copied.

See also the [mise caveats](./mise-caveats) about optional and passed-through
variables.
