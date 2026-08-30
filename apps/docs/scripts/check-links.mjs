/**
 * Fail the build when a link inside the documentation points at nothing.
 *
 * This checks the *built site*, not the Markdown source. That is the whole
 * point: the source links to `*.md` files (so they resolve on GitHub) and a
 * remark plugin turns them into routes, so only the output can tell us whether
 * the two ends actually meet. Starlight's own validator cannot help here — it
 * derives routes from file paths, and this site's content lives outside the
 * app, which it mis-resolves.
 *
 * Links to another host are not our business and are skipped.
 */
import { readFile, readdir, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIST = fileURLToPath(new URL("../dist", import.meta.url));

/** Every `.html` file under `dir`, as absolute paths. */
async function htmlFiles(dir) {
  const found = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) found.push(...(await htmlFiles(full)));
    else if (entry.name.endsWith(".html")) found.push(full);
  }
  return found;
}

/** The route a built file is served at, e.g. `/guides/modules/`. */
function routeOf(file) {
  const rel = path.relative(DIST, file).split(path.sep).join("/");
  return "/" + rel.replace(/index\.html$/, "").replace(/\.html$/, "/");
}

/** Whether a route resolves to something in `dist`. */
async function exists(route) {
  const clean = route.split("?")[0].replace(/^\//, "");
  const candidates = [
    path.join(DIST, clean, "index.html"),
    path.join(DIST, clean),
    path.join(DIST, `${clean.replace(/\/$/, "")}.html`),
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

const files = await htmlFiles(DIST);
const broken = [];

for (const file of files) {
  const html = await readFile(file, "utf8");
  const from = routeOf(file);
  // Only the rendered article: the sidebar and header are generated from the
  // config, so a bad link there is a config bug the build already surfaces.
  const article = html.match(
    /<div class="sl-markdown-content"[\s\S]*?<\/div>/,
  )?.[0];
  if (!article) continue;

  for (const [, href] of article.matchAll(/href="([^"]+)"/g)) {
    if (/^([a-z][a-z0-9+.-]*:|\/\/|#)/i.test(href)) continue;
    const target = new URL(href, `https://baston.dev${from}`);
    if (target.hash && target.pathname === from) continue; // same-page anchor
    if (!(await exists(target.pathname))) {
      broken.push({ from, href });
    }
  }
}

if (broken.length > 0) {
  console.error(`\n✗ ${broken.length} broken link(s):\n`);
  for (const { from, href } of broken) {
    console.error(`  ${from}\n    → ${href}`);
  }
  console.error(
    "\nLinks are written as repository-relative `*.md` paths so they work on\n" +
      "GitHub; check the target moved, not the link syntax.\n",
  );
  process.exit(1);
}

console.log(`✓ links: ${files.length} pages, no broken internal link`);
