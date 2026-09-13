I want to build a CLI tool named `microkitchen` that launches VMs with an environment specified with [Mise](https://mise.jdx.dev/)'s `mise.toml` file (with custom extensions)   
Requirements:   
- By default when invoked it should look for a `mise.toml` file, just like the `mise` CLI tool does.   
- Microkitchen should be implemented in Rust using [the Rust SDK of microsandbox](https://docs.microsandbox.dev/sdk/rust/sandbox)   
- The sandbox must support using Docker inside the sandbox. See their [Docker in a sandbox](https://docs.microsandbox.dev/examples/docker/docker-in-sandbox) example. Use `cruizba/ubuntu-dind:noble-latest` as the base docker image.   
- [mise bootstrap](https://mise.jdx.dev/bootstrap.html) should be used to apply the machine setup   
- files: use `~/.microkitchen` (for state, config, logs, cached files etc.   
- caching: a shared volume should be used for mise caching   
- Microsandbox labels should be used to identify sandboxes created with `microkitchen` and to associate them with the `mise.toml` file on the host system   
- environment variables and secrets:   
    - environment variables (and secrets) should be resolved on the host using `mise`   
    - the configuration of `[microkitchen.secrets]` follows [https://docs.microsandbox.dev/cli/configuration#secrets](https://docs.microsandbox.dev/cli/configuration#secrets)    
    - it is validated on host side that for every secret in that section an environment variable is declared in `[env]`; the value of the environment variable (resolved on the host) should be exposed as a secret using the secrets feature of microsandbox   
    - all environment variables declared in `[env]` for which no matching secret definition in `[microkitchen.secrets]` should be exposed as normal env variables in the sandbox   
    - the secrets should use the passthrough on violation policy for all hosts (`passthrough\_all\_hosts` in the microsandbox Rust SDK)   
- It should be possible to specify configuration in a new `microkitchen` section in `mise.toml`:   
   
```
# the env section is standard mise
[env]
GITHUB_TOKEN = { required = true }
FIGMA_TOKEN = { required = false }
_.file = '.env' # load values from .env file

[microkitchen]
cpus = 4
memory = "10G"
disk = "10G" # root disk file for creating sandbox from OCI image
mounts = ["./src:/app"]

[microkitchen.network]
allow = [
  "example.com",
  "*.microsandbox.dev"
]
ports = ["8000:8000"]

# this references the environment variable; it should
[microkitchen.secrets."GITHUB_TOKEN"]
allow = ["github.com", "*.github.com"]
deny = ["potentiallymalicious.com"]
```
   
- the configuration should be validated before creating or modifying a sandbox   
- Use the following default values: `max\_cpus=64`, `max\_memory=64` (GB)   
- networking:   
    - the network configuration should follow [https://docs.microsandbox.dev/cli/configuration#network](https://docs.microsandbox.dev/cli/configuration#network), but only support the `network`, `allow`, `deny` and `ports` fields   
    - Because microsandbox doesn't allow dynamically altering the network policy after sandbox creation, I want to instead use a socks5 proxy that can be dynamically reconfigured. Please implement one in Rust. Propose a library for implementing one (what is microsandbox using internally? can we use the same technology?)   
    - When a request to an unknown domain is encountered, an interactive popup should be opened offering the following options:   
        - deny → add domain (or ip if domain unavailable) to deny list by modifying the toml file   
        - allow → add domain (or ip if domain unavailable) to allow list by modifying the toml file   
        - allow temporarily → allow connection for the next five minutes   
    - The interactive popup should show the domain (or IP if domain is unavailable) and the process inside the sandbox that is trying to open the connection. This should work for TCP and UDP connections. To find out the domain name, we also need to implement a custom nameserver (proxy) that keeps track of the resolved names and IPs that they map to. See [https://claude.ai/share/feb5d3bb-0c37-4caa-9a9f-b8f8bd861b31](https://claude.ai/share/feb5d3bb-0c37-4caa-9a9f-b8f8bd861b31) for a conversation on how this be done technically    
- modifications: There should be a `microkitchen remodel` command that applies changes to the toml configuration to an existing sandbox. It should display a diff of the changes before applying them. If a change requires a restart, inform the user about this fact and how they can do this. See [https://docs.microsandbox.dev/sandboxes/tuning](https://docs.microsandbox.dev/sandboxes/tuning) for more information   
- There should be integration tests for all important features. Research how the tests are written in [https://github.com/superradcompany/microsandbox/](https://github.com/superradcompany/microsandbox/) and do it similarly   
