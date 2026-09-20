---
layout: home

hero:
  name: microkitchen
  text: microVMs from your mise.toml
  tagline: Docker inside, tools installed with mise bootstrap, secrets resolved on the host, and an egress broker that asks before the sandbox talks to anything new.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: Configuration
      link: /guide/configuration
    - theme: alt
      text: GitHub
      link: https://github.com/timjb/microkitchen

features:
  - title: One file
    details: A <code>[_.microkitchen]</code> table in the project's <code>mise.toml</code> describes the sandbox. mise ignores it; microkitchen finds it the way mise finds its configuration.
    link: /guide/configuration
  - title: Real microVMs
    details: Each project gets its own <a href="https://microsandbox.dev">microsandbox</a> VM with Docker running inside, and its tools installed by <code>mise bootstrap</code>.
    link: /guide/getting-started
  - title: Secrets stay on the host
    details: The guest sees only a placeholder. The real value is substituted into TLS connections to the hosts you allow, and nowhere else.
    link: /guide/env-and-secrets
  - title: Asks before it connects
    details: Every connection to something new opens a dialog showing the destination, the names the sandbox resolved and the process. Your answer is saved to the kitchen file.
    link: /guide/network
---
