// Fails when a package in web/package-lock.json has no licence or one that is
// not on the allowlist. Reads only the lockfile (lockfileVersion 2 or 3
// records each package's `license`), so it needs no installed node_modules
// and no network.
//
// Usage: node scripts/web-licenses.mjs [path/to/package-lock.json]

import { readFileSync } from "node:fs";

// Permissive licences, plus MPL-2.0 (file-level copyleft, compatible with
// an MIT-licensed binary; lightningcss, a CSS build tool, uses it). The same
// policy as deny.toml for Rust crates.
const ALLOWED = new Set([
  "0BSD",
  "Apache-2.0",
  "BlueOak-1.0.0",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "CC-BY-4.0",
  "CC0-1.0",
  "ISC",
  "MIT",
  "MIT-0",
  "MPL-2.0",
  "Python-2.0",
  "Unlicense",
]);

// An SPDX expression is acceptable when one of its OR alternatives consists
// only of allowed licences. Parentheses are flattened, which is exact for the
// `(A OR B)` and `(A AND B)` forms npm packages use.
export function acceptable(expression) {
  if (typeof expression !== "string" || expression.trim() === "") return false;
  const flat = expression.replace(/[()]/g, " ").trim();
  return flat
    .split(/\s+OR\s+/)
    .some((alt) => alt.split(/\s+AND\s+/).every((id) => ALLOWED.has(id.trim())));
}

const path = process.argv[2] ?? new URL("../web/package-lock.json", import.meta.url);
const lock = JSON.parse(readFileSync(path, "utf8"));
if (!lock.packages || (lock.lockfileVersion ?? 0) < 2) {
  console.error("web-licenses: need lockfileVersion 2 or 3 with a `packages` map");
  process.exit(2);
}

const bad = [];
let checked = 0;
for (const [key, pkg] of Object.entries(lock.packages)) {
  if (key === "" || pkg.link) continue; // the project itself, workspace links
  checked += 1;
  const license = typeof pkg.license === "string" ? pkg.license : undefined;
  if (!acceptable(license)) {
    const name = key.replace(/^.*node_modules\//, "");
    bad.push(`${name}@${pkg.version ?? "?"}: ${license ?? "(no licence recorded)"}`);
  }
}

if (bad.length > 0) {
  console.error(`web-licenses: ${bad.length} package(s) not on the allowlist:`);
  for (const line of bad) console.error(`  ${line}`);
  process.exit(1);
}
console.log(`web-licenses: ${checked} packages, all on the allowlist`);
