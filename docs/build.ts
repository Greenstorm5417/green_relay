/**
 * Build a self-contained, statically compiled HTML documentation site from the generated OpenAPI spec.
 *
 * Uses `@redocly/cli` to dynamically compile the OpenAPI spec into a zero-dependency,
 * static HTML file. Output goes to `docs/dist/`.
 *
 * The OpenAPI document is read from $OPENAPI_JSON (default: docs/openapi.json),
 * which the CI pipeline produces via `green_relay openapi`.
 */
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { execSync } from "node:child_process";

const here = import.meta.dir;
const outDir = join(here, "dist");

const specPath = process.env.OPENAPI_JSON ?? join(here, "openapi.json");
if (!existsSync(specPath)) {
  console.error(`OpenAPI spec not found at ${specPath}; generate it first`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });

// Compile the OpenAPI spec into static, standalone HTML
console.log(`Compiling OpenAPI spec from ${specPath} into static HTML...`);
try {
  execSync(`bunx redocly build-docs "${specPath}" -o "${join(outDir, "index.html")}" --title "Green Relay SMS API"`, {
    stdio: "inherit",
    cwd: here,
  });
} catch (err) {
  console.error("Failed to compile OpenAPI docs via redocly:", err);
  process.exit(1);
}

// Ship the raw spec alongside it so external tooling can still consume it.
copyFileSync(specPath, join(outDir, "openapi.json"));

console.log(`built self-contained static HTML docs site in ${outDir}`);
