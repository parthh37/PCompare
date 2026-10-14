#!/usr/bin/env node
// Keeps the app version identical in package.json, Cargo.toml and
// tauri.conf.json — the updater compares the installed version against
// latest.json, so a tag that disagrees with the bundled version would offer
// the same "update" forever.
//
//   node scripts/version.mjs bump 0.2.0   rewrite all three files
//   node scripts/version.mjs check v0.2.0 fail unless all three equal the tag
//   node scripts/version.mjs next minor   print the version a release would get
//                                         (as-is | patch | minor | major | x.y.z)
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

// The first "version" key indented by exactly two spaces is the top-level one.
const JSON_VERSION = /^(  "version":\s*")([^"]+)(")/m;

function current() {
  return {
    package: JSON.parse(readFileSync(files.package, "utf8")).version,
    cargo: readFileSync(files.cargo, "utf8").match(CARGO_VERSION)?.[2],
    tauri: JSON.parse(readFileSync(files.tauri, "utf8")).version,
  };
}

function bump(version) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) throw new Error(`not a semver version: ${version}`);
  // Edit the top-level "version" line in place rather than re-serialising, so
  // hand-formatted JSON (one-line arrays in tauri.conf.json) stays untouched.
  for (const key of ["package", "tauri"]) {
    const text = readFileSync(files[key], "utf8");
    if (!JSON_VERSION.test(text)) throw new Error(`no top-level "version" in ${files[key]}`);
    writeFileSync(files[key], text.replace(JSON_VERSION, `$1${version}$3`));
  }
  const cargo = readFileSync(files.cargo, "utf8");
  if (!CARGO_VERSION.test(cargo)) throw new Error("no [workspace.package] version in Cargo.toml");
  writeFileSync(files.cargo, cargo.replace(CARGO_VERSION, `$1${version}$3`));
  console.log(`version set to ${version} — run cargo check to refresh Cargo.lock`);
}

// The version a release of kind `kind` gets, starting from `from`.
// TODO(you): prerelease handling is a policy choice — see next() notes in the PR.
function next(from, kind) {
  const m = from.match(/^(\d+)\.(\d+)\.(\d+)(-.+)?$/);
  if (!m) throw new Error(`current version isn't semver: ${from}`);
  const [major, minor, patch] = m.slice(1, 4).map(Number);
  const pre = Boolean(m[4]);
  switch (kind) {
    case "as-is": return from;
    // A prerelease bumps to its own final version first (0.4.0-beta.1 → 0.4.0),
    // the same rule `npm version` follows.
    case "patch": return pre ? `${major}.${minor}.${patch}` : `${major}.${minor}.${patch + 1}`;
    case "minor": return pre && patch === 0 ? `${major}.${minor}.0` : `${major}.${minor + 1}.0`;
    case "major": return pre && minor === 0 && patch === 0 ? `${major}.0.0` : `${major + 1}.0.0`;
    default:
      if (/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(kind)) return kind;
      throw new Error(`expected as-is, patch, minor, major or x.y.z — got ${kind}`);
  }
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
else if (cmd === "next" && arg) console.log(next(current().package, arg));
else if (cmd === "show") console.log(current());
else {
  console.error("usage: version.mjs bump <x.y.z> | check <vX.Y.Z> | next <kind> | show");
  process.exit(2);
}
