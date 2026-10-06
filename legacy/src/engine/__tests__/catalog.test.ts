import { describe, expect, test } from "bun:test";

import { z } from "zod";

import {
  buildCatalog,
  defineCommand,
  defineModule,
  renderMarkdownReference,
} from "../index.ts";

describe("command catalog", () => {
  test("drives discovery and generated Markdown", () => {
    const command = defineCommand({
      examples: ["demo widgets list --json"],
      outputSchema: z.array(z.object({ id: z.string() })),
      path: ["widgets", "list"],
      run() {
        return { data: [{ id: "widget-1" }] };
      },
      summary: "List widgets",
    });
    const module = defineModule({
      commands: [command],
      id: "widgets",
      summary: "Manage widgets",
    });
    const { catalog } = buildCatalog([module]);

    expect(catalog.find(["widgets", "list"])?.summary).toBe("List widgets");
    expect(catalog.toJSON().schemaVersion).toBe(2);
    expect(
      renderMarkdownReference(
        { description: "Demo CLI", name: "demo", version: "1.0.0" },
        catalog
      )
    ).toContain("demo widgets list --json");
  });

  test("rejects duplicate command paths", () => {
    const command = defineCommand({
      path: ["widgets", "list"],
      run() {
        return { data: [] };
      },
      summary: "List widgets",
    });
    expect(() =>
      buildCatalog([
        defineModule({
          commands: [command],
          id: "one",
          summary: "First",
        }),
        defineModule({
          commands: [command],
          id: "two",
          summary: "Second",
        }),
      ])
    ).toThrow('Command "widgets list"');
  });
});
