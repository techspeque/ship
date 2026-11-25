// Fails when a package in web/package-lock.json has no licence or one that is
// not on the allowlist. Reads only the lockfile (lockfileVersion 2 or 3
// records each package's `license`), so it needs no installed node_modules
// and no network.
//
// Usage: node scripts/web-licenses.mjs [path/to/package-lock.json]
//        node scripts/web-licenses.mjs --self-test

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

// An SPDX expression is acceptable when it evaluates true with each allowed
// licence id true and every other id false: `A OR B` needs either side,
// `A AND B` needs both, AND binds tighter than OR, parentheses group.
// Anything else (WITH exceptions, `+`, LicenseRef, unbalanced parentheses,
// a stray operator) does not parse and is rejected.
export function acceptable(expression) {
  if (typeof expression !== "string") return false;
  const tokens = expression.match(/\(|\)|[^\s()]+/g) ?? [];
  let i = 0;
  const isOp = (t) => t === "AND" || t === "OR" || t === "WITH";
  // Each parse function returns the expression's value, or null when it
  // does not parse.
  const atom = () => {
    const t = tokens[i];
    if (t === undefined || t === ")" || isOp(t)) return null;
    i += 1;
    if (t === "(") {
      const v = orExpr();
      if (v === null || tokens[i] !== ")") return null;
      i += 1;
      return v;
    }
    if (!/^[A-Za-z0-9.-]+$/.test(t)) return null;
    return ALLOWED.has(t);
  };
  const andExpr = () => {
    let v = atom();
    while (v !== null && tokens[i] === "AND") {
      i += 1;
      const r = atom();
      v = r === null ? null : v && r;
    }
    return v;
  };
  const orExpr = () => {
    let v = andExpr();
    while (v !== null && tokens[i] === "OR") {
      i += 1;
      const r = andExpr();
      v = r === null ? null : v || r;
    }
    return v;
  };
  const value = orExpr();
  return value === true && i === tokens.length;
}

// Regressions for acceptable(), run by CI with --self-test.
const CASES = [
  ["MIT", true],
  ["GPL-3.0-only", false],
  ["(MIT OR Apache-2.0)", true],
  ["(MIT AND Apache-2.0)", true],
  ["MIT OR GPL-3.0-only", true],
  ["MIT AND GPL-3.0-only", false],
  ["(MIT OR Apache-2.0) AND GPL-3.0-only", false],
  ["GPL-3.0-only AND (MIT OR Apache-2.0)", false],
  ["(MIT OR GPL-3.0-only) AND Apache-2.0", true],
  ["MIT OR GPL-3.0-only AND Apache-2.0", true],
  ["MIT AND GPL-3.0-only OR Apache-2.0", true],
  ["(MIT AND GPL-3.0-only) OR ISC", true],
  ["MIT AND (GPL-3.0-only OR ISC)", true],
  ["Apache-2.0 WITH LLVM-exception", false],
  ["MIT+", false],
  ["(MIT OR Apache-2.0", false],
  ["MIT OR", false],
  ["MIT Apache-2.0", false],
  ["()", false],
  ["", false],
  [undefined, false],
];

if (process.argv[2] === "--self-test") {
  const failed = CASES.filter(([expr, want]) => acceptable(expr) !== want);
  for (const [expr, want] of failed) {
    console.error(`web-licenses self-test: acceptable(${JSON.stringify(expr)}) should be ${want}`);
  }
  if (failed.length > 0) process.exit(1);
  console.log(`web-licenses self-test: ${CASES.length} cases passed`);
  process.exit(0);
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
