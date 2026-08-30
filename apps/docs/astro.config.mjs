// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import { fileURLToPath } from "node:url";

import { checkLinks } from "./src/plugins/check-links.mjs";
import { rewriteDocLinks } from "./src/plugins/rewrite-doc-links.mjs";

const DOCS_ROOT = fileURLToPath(new URL("../../docs", import.meta.url));

/**
 * The BASTON documentation website.
 *
 * It renders `docs/` at the repository root — it does not own a copy. The
 * Markdown has to stay readable on GitHub, so this app reaches out to it
 * rather than pulling it in; see `src/content.config.ts` for the loader.
 */
export default defineConfig({
  site: "https://baston.dev",
  markdown: {
    // Authors write GitHub-correct `*.md` links; these become site routes.
    remarkPlugins: [[rewriteDocLinks, { docsRoot: DOCS_ROOT }]],
  },
  srcDir: "./src",
  integrations: [
    starlight({
      title: "BASTON",
      description:
        "A from-scratch FiveM server core, written in Rust. Not a fork of the C++ FXServer.",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/shiplabs/baston",
        },
      ],
      editLink: {
        baseUrl: "https://github.com/shiplabs/baston/edit/develop/docs/",
      },
      // The sidebar is written by hand rather than generated from the tree:
      // the order pages should be *read* in is not the order they sort in.
      sidebar: [
        { label: "Overview", link: "/" },
        {
          label: "Run a server",
          items: [
            { label: "Start here", link: "/server/" },
            { label: "Quickstart", link: "/server/quickstart/" },
            { label: "Installing resources", link: "/server/resources/" },
            { label: "Modules and bundles", link: "/server/modules/" },
            { label: "Voice", link: "/server/voice/" },
            { label: "Streaming assets", link: "/server/streaming/" },
            { label: "Multi-zone", link: "/server/multi-zone/" },
            { label: "Zone configuration", link: "/server/zone-config/" },
            { label: "displayinfo overlay", link: "/server/displayinfo/" },
            { label: "Monitoring", link: "/server/monitoring/" },
            { label: "Going public", link: "/server/going-public/" },
            { label: "Troubleshooting", link: "/server/troubleshooting/" },
          ],
        },
        {
          label: "Write resources",
          items: [
            { label: "Start here", link: "/scripting/" },
            { label: "Your first resource", link: "/scripting/your-first-resource/" },
            { label: "Events", link: "/scripting/events/" },
            { label: "Natives", link: "/scripting/natives/" },
            { label: "State bags and KVP", link: "/scripting/state-bags/" },
            { label: "Using a database", link: "/scripting/database/" },
            { label: "HTTP", link: "/scripting/http/" },
            { label: "Coming from FXServer", link: "/scripting/from-fivem/" },
          ],
        },
        {
          label: "Develop BASTON",
          items: [
            { label: "Architecture", link: "/develop/" },
            { label: "The crates", link: "/develop/crates/" },
            { label: "Adding a native", link: "/develop/adding-a-native/" },
            { label: "Adding a module", link: "/develop/adding-a-module/" },
            { label: "Testing", link: "/develop/testing/" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Configuration", link: "/reference/configuration/" },
            { label: "Metrics", link: "/reference/metrics/" },
            { label: "Monitoring & control API", link: "/reference/api/" },
            { label: "Native coverage", link: "/reference/natives-gap/" },
          ],
        },
        {
          label: "Internals",
          items: [
            { label: "The wire protocol", link: "/internals/protocol/" },
            { label: "State synchronisation", link: "/internals/state-sync/" },
            { label: "CFX platform handshake", link: "/internals/cfx-platform-handshake/" },
            { label: "Code quality audit", link: "/internals/code-quality-audit-2026-07-05/" },
            { label: "Getting started (FR, original)", link: "/internals/getting-started-fr/" },
          ],
        },
        {
          label: "Operations",
          items: [
            { label: "Running BASTON", link: "/operations/running/" },
            { label: "CFX licensing", link: "/operations/licensing/" },
            { label: "Asset escrow", link: "/operations/escrow/" },
            {
              label: "Runbooks",
              items: [
                { label: "Local live test", link: "/operations/runbooks/local-live-test/" },
                { label: "Phase C", link: "/operations/runbooks/phase-c/" },
                { label: "Phase C testing", link: "/operations/runbooks/phase-c-testing/" },
              ],
            },
          ],
        },
        { label: "Decisions (ADR)", autogenerate: { directory: "adr" } },
      ],
      customCss: ["./src/styles/baston.css"],
      lastUpdated: true,
      pagination: true,
    }),
    // Declared last on purpose: `astro:build:done` hooks fire in this
    // order, and Starlight's writes the sitemap and search index that the
    // check would otherwise report as missing.
    checkLinks(),
  ],
});
