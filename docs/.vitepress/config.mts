import { readFileSync } from "node:fs";
import { defineConfig } from "vitepress";

// The version shown in the nav comes from the workspace's Cargo.toml.
const cargoToml = readFileSync(
  new URL("../../Cargo.toml", import.meta.url),
  "utf8",
);
const version =
  cargoToml.match(/^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1] ??
  "0.0.0";

const repo = "https://github.com/timjb/microkitchen";

// https://vitepress.dev/reference/site-config
export default defineConfig({
  title: "microkitchen",
  description:
    "Launch microsandbox microVMs from a mise.toml, with an egress broker that asks before the sandbox talks to anything new.",
  lang: "en-US",
  // Served from GitHub Pages at https://timjb.github.io/microkitchen/.
  base: "/microkitchen/",
  cleanUrls: true,
  lastUpdated: true,
  themeConfig: {
    outline: "deep",
    nav: [
      { text: "Guide", link: "/guide/getting-started" },
      { text: "Reference", link: "/reference/commands" },
      { text: `v${version}`, link: `${repo}/blob/main/Cargo.toml` },
    ],
    sidebar: [
      {
        text: "Guide",
        items: [
          { text: "Getting started", link: "/guide/getting-started" },
          { text: "Configuration", link: "/guide/configuration" },
          { text: "The sandbox user: chef", link: "/guide/chef" },
          { text: "Environment and secrets", link: "/guide/env-and-secrets" },
          { text: "Network access", link: "/guide/network" },
          { text: "Changing a sandbox", link: "/guide/remodel" },
          { text: "mise caveats", link: "/guide/mise-caveats" },
        ],
      },
      {
        text: "Reference",
        items: [
          { text: "Commands", link: "/reference/commands" },
          { text: "Settings", link: "/reference/settings" },
          { text: "Files", link: "/reference/files" },
        ],
      },
      { text: "Development", link: "/development" },
    ],
    socialLinks: [{ icon: "github", link: repo }],
    editLink: {
      pattern: `${repo}/edit/main/docs/:path`,
    },
    search: { provider: "local" },
  },
});
