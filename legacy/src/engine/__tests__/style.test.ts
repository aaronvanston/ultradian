import { describe, expect, test } from "bun:test";

import { createUi, stripAnsi } from "../index.ts";

describe("terminal presentation", () => {
  test("uses a stable, distinct HTTP method palette", () => {
    const ui = createUi({
      colorMode: "always",
      env: {},
      machine: false,
      streamIsTty: true,
    });
    const rendered = ["GET", "POST", "PUT", "PATCH", "DELETE"].map((method) =>
      ui.httpMethod(method)
    );

    expect(new Set(rendered).size).toBe(5);
    expect(rendered.every((value) => value.includes("\u001B["))).toBe(true);
    expect(stripAnsi(ui.httpMethodPadded("GET"))).toBe("GET    ");
  });

  test("keeps method labels intact when color is disabled", () => {
    const ui = createUi({
      colorMode: "auto",
      env: { NO_COLOR: "1" },
      machine: false,
      streamIsTty: true,
    });

    expect(ui.httpMethod("DELETE")).toBe("DELETE");
    expect(ui.httpMethodPadded("POST")).toBe("POST   ");
  });
});
