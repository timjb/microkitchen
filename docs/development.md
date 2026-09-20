# Development

```sh
just test              # unit tests and VM-free integration tests
just lint              # rustfmt and clippy, warnings are errors
just check             # both (what CI runs on hosted runners)
just test-scripts      # the guest attribution script against fake /proc trees
just test-integration  # VM tests: need KVM, msb and cargo-nextest
just test-vm           # VM tests with cargo test, one at a time
```

The VM tests boot real sandboxes against the internet; with
`MK_TEST_ISOLATE_HOME=1` they use their own microsandbox home. Their kitchens
use one vCPU: in nested virtualization, 2-vCPU guests were several times slower
to boot and bootstrap.

Design and plan:
[`specs/egress-broker-design.md`](https://github.com/timjb/microkitchen/blob/main/specs/egress-broker-design.md),
[`specs/implementation-plan.md`](https://github.com/timjb/microkitchen/blob/main/specs/implementation-plan.md).

## This documentation

The site is built with [VitePress](https://vitepress.dev) from the Markdown in
`docs/`. [mise](https://mise.jdx.dev) installs Node.js and pnpm:

```sh
mise run docs:dev      # serve with live reload
mise run docs:build    # build into docs/.vitepress/dist
```

Pushes to `main` that touch the docs deploy the site to GitHub Pages.
