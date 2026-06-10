/**
 * Build a self-contained Swagger UI site from the generated OpenAPI spec.
 *
 * Copies the Swagger UI assets from the `swagger-ui-dist` package (so nothing
 * is loaded from a CDN at runtime) and emits an `index.html` that renders the
 * spec served alongside it. Output goes to `docs/dist/`.
 *
 * The OpenAPI document is read from $OPENAPI_JSON (default: docs/openapi.json),
 * which the CI pipeline produces via `green_relay openapi`.
 */
import { copyFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import SwaggerUIDist from "swagger-ui-dist";

const here = import.meta.dir;
const outDir = join(here, "dist");

// Resolve the directory holding the prebuilt Swagger UI assets.
const dist = SwaggerUIDist as unknown as {
  absolutePath?: () => string;
  getAbsoluteFSPath?: () => string;
};
const assetsDir = dist.absolutePath?.() ?? dist.getAbsoluteFSPath?.();
if (!assetsDir) {
  console.error("could not locate swagger-ui-dist assets");
  process.exit(1);
}

const specPath = process.env.OPENAPI_JSON ?? join(here, "openapi.json");
if (!existsSync(specPath)) {
  console.error(`OpenAPI spec not found at ${specPath}; generate it first`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });

// Self-host the Swagger UI assets the page references.
const assets = [
  "swagger-ui.css",
  "swagger-ui-bundle.js",
  "swagger-ui-standalone-preset.js",
  "favicon-32x32.png",
  "favicon-16x16.png",
];
for (const file of assets) {
  copyFileSync(join(assetsDir, file), join(outDir, file));
}

// Ship the raw spec so external tooling can consume it too.
copyFileSync(specPath, join(outDir, "openapi.json"));

const html = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Green Relay SMS API</title>
    <link rel="stylesheet" href="./swagger-ui.css" />
    <link rel="icon" type="image/png" href="./favicon-32x32.png" sizes="32x32" />
    <style>
      body {
        margin: 0;
        background: #fafafa;
      }
    </style>
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="./swagger-ui-bundle.js"></script>
    <script src="./swagger-ui-standalone-preset.js"></script>
    <script>
      window.addEventListener("load", () => {
        window.ui = SwaggerUIBundle({
          url: "./openapi.json",
          dom_id: "#swagger-ui",
          deepLinking: true,
          presets: [SwaggerUIBundle.presets.apis, SwaggerUIStandalonePreset],
          layout: "StandaloneLayout",
        });
      });
    </script>
  </body>
</html>
`;
writeFileSync(join(outDir, "index.html"), html);

console.log(`built self-contained Swagger UI site in ${outDir}`);
