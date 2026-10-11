#!/usr/bin/env node
// Keeps the app version identical in package.json, Cargo.toml and
// tauri.conf.json — the updater compares the installed version against
// latest.json, so a tag that disagrees with the bundled version would offer
// the same "update" forever.
//
//   node scripts/version.mjs bump 0.2.0   rewrite all three files
//   node scripts/version.mjs check v0.2.0 fail unless all three equal the tag
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const files = {
  package: `${root}package.json`,
  cargo: `${root}Cargo.toml`,
  tauri: `${root}src-tauri/tauri.conf.json`,
};
// Only the [workspace.package] version — dependency versions must not match.
const CARGO_VERSION = /(\[workspace\.package\][^[]*?\nversion\s*=\s*")([^"]+)(")/;

function current() {
  return {
    package: JSON.parse(readFileSync(files.package, "utf8")).version,
    cargo: readFileSync(files.cargo, "utf8").match(CARGO_VERSION)?.[2],
    tauri: JSON.parse(readFileSync(files.tauri, "utf8")).version,
  };
}

function bump(version) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) throw new Error(`not a semver version: ${version}`);
  for (const key of ["package", "tauri"]) {
    const json = JSON.parse(readFileSync(files[key], "utf8"));
    json.version = version;
    writeFileSync(files[key], JSON.stringify(json, null, 2) + "\n");
  }
  const cargo = readFileSync(files.cargo, "utf8");
  if (!CARGO_VERSION.test(cargo)) throw new Error("no [workspace.package] version in Cargo.toml");
  writeFileSync(files.cargo, cargo.replace(CARGO_VERSION, `$1${version}$3`));
  console.log(`version set to ${version} — run cargo check to refresh Cargo.lock`);
}

function check(tag) {
  const want = tag.replace(/^v/, "");
  const found = current();
  const bad = Object.entries(found).filter(([, v]) => v !== want);
  if (bad.length) {
    for (const [file, v] of bad) console.error(`${file}: ${v ?? "missing"} (expected ${want})`);
    console.error(`Run: node scripts/version.mjs bump ${want}`);
    process.exit(1);
  }
  console.log(`all versions are ${want}`);
}

const [cmd, arg] = process.argv.slice(2);
if (cmd === "bump" && arg) bump(arg);
else if (cmd === "check" && arg) check(arg);
else if (cmd === "show") console.log(current());
else {
  console.error("usage: version.mjs bump <x.y.z> | check <vX.Y.Z> | show");
  process.exit(2);
}
