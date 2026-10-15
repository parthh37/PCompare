// Assembles the GitHub Pages site: site/ plus the README's screenshots and the app icon,
// so the product page never carries its own copies of them.
//
//   node scripts/build-site.mjs [outDir]    (default: _site)
import { cpSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const out = resolve(root, process.argv[2] ?? "_site");

rmSync(out, { recursive: true, force: true });
cpSync(join(root, "site"), out, { recursive: true });
cpSync(join(root, "docs/screenshots"), join(out, "screenshots"), { recursive: true });
cpSync(join(root, "app-icon.svg"), join(out, "favicon.svg"));

console.log(`site written to ${out}`);
