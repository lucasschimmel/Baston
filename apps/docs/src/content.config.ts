import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
import { docsSchema } from "@astrojs/starlight/schema";

/**
 * The site reads `docs/` at the repository root directly.
 *
 * Starlight's default loader expects content inside this app. Pointing a glob
 * loader outside it instead is what keeps one copy of the documentation:
 * `docs/` stays browsable on GitHub, and this app renders the same bytes.
 */
export const collections = {
  docs: defineCollection({
    loader: glob({
      pattern: ["**/*.md", "!**/node_modules/**"],
      base: "../../docs",
      // `docs/README.md` is what GitHub shows when you open the folder, so it
      // is the site's index too — one page, both audiences.
      generateId: ({ entry }) =>
        entry.replace(/\.md$/, "").replace(/(^|\/)README$/, "$1index"),
    }),
    schema: docsSchema(),
  }),
};
