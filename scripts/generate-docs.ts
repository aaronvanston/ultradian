import { mkdir } from "node:fs/promises";
import path from "node:path";

import { app } from "../src/app.ts";
import { renderMarkdownReference } from "../src/engine/index.ts";

const docsDirectory = path.resolve(import.meta.dir, "../docs");
const outputPath = path.resolve(docsDirectory, "commands.md");
await mkdir(docsDirectory, { recursive: true });
await Bun.write(outputPath, renderMarkdownReference(app.meta, app.catalog));
console.log(`generated ${outputPath}`);
