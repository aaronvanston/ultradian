import { describe, expect, test } from "bun:test";

import { createServiceToken, ServiceRegistry } from "../index.ts";

describe("ServiceRegistry", () => {
  test("constructs lazily once and disposes in reverse order", async () => {
    const events: string[] = [];
    const database = createServiceToken<{ id: string }>("database");
    const client = createServiceToken<{ databaseId: string }>("client");
    const services = new ServiceRegistry([
      {
        create() {
          events.push("create database");
          return { id: "db-1" };
        },
        dispose() {
          events.push("dispose database");
        },
        token: database,
      },
      {
        async create(registry) {
          events.push("create client");
          const value = await registry.get(database);
          return { databaseId: value.id };
        },
        dispose() {
          events.push("dispose client");
        },
        token: client,
      },
    ]);

    expect(events).toEqual([]);
    const resolvedClient = await services.get(client);
    expect(resolvedClient.databaseId).toBe("db-1");
    expect(await services.get(database)).toEqual({ id: "db-1" });
    await services.dispose();
    expect(events).toEqual([
      "create client",
      "create database",
      "dispose client",
      "dispose database",
    ]);
  });

  test("reports dependency cycles by service name", () => {
    const first = createServiceToken<string>("first");
    const second = createServiceToken<string>("second");
    const services = new ServiceRegistry([
      {
        async create(registry) {
          return await registry.get(second);
        },
        token: first,
      },
      {
        async create(registry) {
          return await registry.get(first);
        },
        token: second,
      },
    ]);

    expect(services.get(first)).rejects.toMatchObject({
      code: "service_cycle",
    });
  });
});
