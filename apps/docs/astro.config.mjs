// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import { fileURLToPath } from "node:url";

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
          label: "Guides",
          items: [
            { label: "Getting started", link: "/guides/getting-started/" },
            { label: "Modules and bundles", link: "/guides/modules/" },
            { label: "Zone configuration", link: "/guides/zone-config/" },
            { label: "Streaming assets", link: "/guides/streaming/" },
            { label: "displayinfo overlay", link: "/guides/displayinfo/" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Monitoring & control API", link: "/reference/api/" },
            { label: "Native coverage", link: "/reference/natives-gap/" },
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
                {
                  label: "Local live test",
                  link: "/operations/runbooks/local-live-test/",
                },
                { label: "Phase C", link: "/operations/runbooks/phase-c/" },
                {
                  label: "Phase C testing",
                  link: "/operations/runbooks/phase-c-testing/",
                },
              ],
            },
          ],
        },
        {
          label: "Internals",
          items: [
            {
              label: "CFX platform handshake",
              link: "/internals/cfx-platform-handshake/",
            },
            {
              label: "Code quality audit",
              link: "/internals/code-quality-audit-2026-07-05/",
            },
          ],
        },
        { label: "Decisions (ADR)", autogenerate: { directory: "adr" } },
      ],
      customCss: ["./src/styles/baston.css"],
      lastUpdated: true,
      pagination: true,
    }),
  ],
});
