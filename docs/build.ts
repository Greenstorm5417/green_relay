/**
 * Build a self-contained, statically compiled HTML documentation site from the generated OpenAPI spec.
 *
 * Uses `@redocly/cli` to dynamically compile the OpenAPI spec into a zero-dependency,
 * static HTML file. Output goes to `docs/dist/`.
 *
 * The OpenAPI document is read from $OPENAPI_JSON (default: docs/openapi.json),
 * which the CI pipeline produces via `green_relay openapi`.
 */
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
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

const outFile = join(outDir, "index.html");

// Compile the OpenAPI spec into static HTML. `redocly build-docs` server-side
// renders the full documentation content into the page.
console.log(`Compiling OpenAPI spec from ${specPath} into static HTML...`);
try {
  execSync(
    `bunx redocly build-docs "${specPath}" -o "${outFile}" --title "Green Relay SMS API"`,
    { stdio: "inherit", cwd: here },
  );
} catch (err) {
  console.error("Failed to compile OpenAPI docs via redocly:", err);
  process.exit(1);
}

// build-docs pre-renders the content but also injects the Redoc runtime bundle
// (loaded from a CDN) plus a hydration-state script that turns the page back
// into a client-rendered React app. Strip every <script> block so the result
// is fully pre-rendered, JS-free, and has no external dependencies — readable
// by crawlers and LLMs while keeping Redoc's inlined styling and layout.
const rendered = readFileSync(outFile, "utf8");
const stripped = rendered.replace(/<script\b[\s\S]*?<\/script>/gi, "");
writeFileSync(outFile, stripped, "utf8");
console.log(
  `stripped ${
    (rendered.match(/<script\b/gi) ?? []).length
  } script block(s); docs are now fully pre-rendered`,
);

// Ship the raw spec alongside it so external tooling can still consume it.
copyFileSync(specPath, join(outDir, "openapi.json"));

console.log(`built pre-rendered static HTML docs site in ${outDir}`);
