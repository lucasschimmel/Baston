import { readFile, readdir, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Fail the build when a link inside the documentation points at nothing.
 *
 * An Astro integration rather than a separate script, so it runs on every
 * build — `astro build`, `bun run docs:build`, CI — with no wrapper anyone can
 * forget or bypass.
 *
 * It checks the *built site*, not the Markdown source. That is the point: the
 * source links to `*.md` files so they resolve on GitHub, and a remark plugin
 * rewrites them into routes, so only the output can tell us whether the two
 * ends meet. Starlight's own validator cannot do this job — it derives routes
 * from file paths, and this site's content lives outside the app, which it
 * mis-resolves.
 *
 * Links to another host are not our business and are skipped.
 */
export function checkLinks() {
  return {
    name: "baston-check-links",
    hooks: {
      "astro:build:done": async ({ dir, logger }) => {
        const distRoot = fileURLToPath(dir);

        /** Every `.html` file under `dir`, as absolute paths. */
        async function htmlFiles(current) {
          const found = [];
          for (const entry of await readdir(current, { withFileTypes: true })) {
            const full = path.join(current, entry.name);
            if (entry.isDirectory()) found.push(...(await htmlFiles(full)));
            else if (entry.name.endsWith(".html")) found.push(full);
          }
          return found;
        }

        /** The route a built file is served at, e.g. `/server/quickstart/`. */
        function routeOf(file) {
          const rel = path.relative(distRoot, file).split(path.sep).join("/");
          return "/" + rel.replace(/index\.html$/, "").replace(/\.html$/, "/");
        }

        /** Whether a route resolves to something in the output. */
        async function exists(route) {
          const clean = route.split("?")[0].replace(/^\//, "");
          const candidates = [
            path.join(distRoot, clean, "index.html"),
            path.join(distRoot, clean),
            path.join(distRoot, `${clean.replace(/\/$/, "")}.html`),
          ];
          for (const candidate of candidates) {
            try {
              if ((await stat(candidate)).isFile()) return true;
            } catch {
              // Try the next shape.
            }
          }
          return false;
        }

        const files = await htmlFiles(distRoot);
        const broken = [];

        // Every internal link on the page, chrome included. An earlier version
        // tried to isolate the rendered article with a regex and stopped at
        // the first `</div>`, so it silently checked only the top of each
        // page. The sidebar is hand-written here anyway, so a dead nav entry
        // is worth failing on too.
        for (const file of files) {
          const html = await readFile(file, "utf8");
          const from = routeOf(file);
          const seen = new Set();

          for (const [, href] of html.matchAll(/href="([^"]+)"/g)) {
            if (/^([a-z][a-z0-9+.-]*:|\/\/|#)/i.test(href)) continue;
            if (seen.has(href)) continue;
            seen.add(href);

            const target = new URL(href, `https://baston.dev${from}`);
            if (target.hash && target.pathname === from) continue;
            if (!(await exists(target.pathname))) broken.push({ from, href });
          }
        }

        if (broken.length > 0) {
          for (const { from, href } of broken) {
            logger.error(`${from} → ${href}`);
          }
          throw new Error(
            `${broken.length} broken documentation link(s). Links are written ` +
              "as repository-relative `*.md` paths so they work on GitHub; " +
              "check whether the target moved, not the link syntax.",
          );
        }

        logger.info(`links: ${files.length} pages, no broken internal link`);
      },
    },
  };
}
