import { AppError } from "./errors.ts";

const serviceType = Symbol("serviceType");

type BivariantCallback<TArguments extends unknown[], TResult> = {
  // Service providers are heterogeneous once stored in the registry.
  // oxlint-disable-next-line typescript/method-signature-style -- method syntax supplies the required callback bivariance at that boundary.
  bivarianceHack(...arguments_: TArguments): TResult;
}["bivarianceHack"];

export interface ServiceToken<T> {
  readonly id: symbol;
  readonly name: string;
  readonly [serviceType]?: T;
}

export interface ServiceProvider<T> {
  readonly token: ServiceToken<T>;
  create: (services: ServiceRegistry) => T | Promise<T>;
  dispose?: BivariantCallback<[value: T], void | Promise<void>>;
}

interface ServiceRecord<T = unknown> {
  provider: ServiceProvider<T>;
  instance?: Promise<T>;
  value?: T;
}

export const createServiceToken = <T>(name: string): ServiceToken<T> =>
  Object.freeze({ id: Symbol(name), name });

export const provideValue = <T>(
  token: ServiceToken<T>,
  value: T
): ServiceProvider<T> => ({
  create: () => value,
  token,
});

export class ServiceRegistry {
  readonly #records = new Map<symbol, ServiceRecord>();
  readonly #disposalOrder: (() => void | Promise<void>)[] = [];
  readonly #resolving = new Set<symbol>();

  constructor(providers: readonly ServiceProvider<unknown>[]) {
    for (const provider of providers) {
      this.#records.set(provider.token.id, { provider });
    }
  }

  async get<T>(token: ServiceToken<T>): Promise<T> {
    // The token symbol is the runtime binding between T and its provider.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Map storage intentionally erases that generic at the registry boundary.
    const record = this.#records.get(token.id) as ServiceRecord<T> | undefined;
    if (!record) {
      throw new AppError({
        code: "service_not_registered",
        message: `Service "${token.name}" is not registered.`,
      });
    }

    if (record.value !== undefined) {
      return record.value;
    }

    if (record.instance !== undefined) {
      return await record.instance;
    }

    if (this.#resolving.has(token.id)) {
      throw new AppError({
        code: "service_cycle",
        message: `Circular service dependency while resolving "${token.name}".`,
      });
    }

    this.#resolving.add(token.id);
    record.instance = Promise.resolve(record.provider.create(this));
    try {
      const value = await record.instance;
      record.value = value;
      const { dispose } = record.provider;
      if (dispose !== undefined) {
        this.#disposalOrder.push(async () => {
          await dispose(value);
        });
      }
      return value;
    } finally {
      this.#resolving.delete(token.id);
    }
  }

  async dispose(): Promise<void> {
    const errors: unknown[] = [];
    for (const dispose of this.#disposalOrder.toReversed()) {
      try {
        // Disposal is deliberately sequential so dependencies remain alive
        // until the services that use them have finished cleaning up.
        // oxlint-disable-next-line no-await-in-loop -- reverse-order lifecycle semantics require sequential awaits.
        await dispose();
      } catch (error) {
        errors.push(error);
      }
    }
    this.#disposalOrder.length = 0;
    if (errors.length > 0) {
      throw new AggregateError(
        errors,
        "One or more services failed to dispose."
      );
    }
  }
}
