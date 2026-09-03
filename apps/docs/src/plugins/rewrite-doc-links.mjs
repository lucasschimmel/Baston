import path from "node:path";
import { visit } from "unist-util-visit";

/**
 * Rewrite repository-relative `*.md` links to site routes.
 *
 * The Markdown in `docs/` has to work in two places at once. On GitHub a link
 * is a path to a file (`../adr/002-module-tiers.md`); on the site it is a
 * route (`/adr/002-module-tiers/`). Authors should write the GitHub form —
 * it is the one that can be checked by opening the file — and this plugin
 * converts it at build time.
 *
 * The conversion is not a suffix strip. A page authored at
 * `docs/guides/modules.md` is served from `/guides/modules/`, one directory
 * deeper than the file, so every `../` in the source would land one level
 * short. Links are therefore resolved against the *file's* directory and
 * emitted as absolute site paths, which is level-independent.
 *
 * @param {{ docsRoot: string }} options
 */
export function rewriteDocLinks({ docsRoot }) {
  const root = path.resolve(docsRoot);

  return (tree, file) => {
    const fileDir = path.dirname(path.resolve(file.path ?? root));

    visit(tree, "link", (node) => {
      const url = node.url;
      if (!url) return;
      // Absolute, protocol-relative, external, or a bare anchor: not ours.
      if (/^([a-z][a-z0-9+.-]*:|\/\/|\/|#)/i.test(url)) return;

      const [target, hash] = url.split("#");
      if (!target.endsWith(".md")) return;

      const absolute = path.resolve(fileDir, target);
      const relative = path.relative(root, absolute).split(path.sep).join("/");
      // A link that escapes `docs/` points at the repository, not the site;
      // leave it alone rather than inventing a route for it.
      if (relative.startsWith("..")) return;

      let route = relative.replace(/\.md$/, "").replace(/(^|\/)README$/i, "$1");
      route = route ? `/${route}/` : "/";
      node.url = hash ? `${route}#${hash}` : route;
    });
  };
}
