declare module "node:test" {
  export type TestFn = () => void | Promise<void>;
  export function describe(name: string, fn: () => void): void;
  export function it(name: string, fn: TestFn): void;
  export function test(name: string, fn: TestFn): void;
}

declare module "node:assert/strict" {
  export function ok(value: unknown, message?: string | Error): asserts value;
  export function equal<T>(actual: unknown, expected: T, message?: string | Error): asserts actual is T;
  export function strictEqual<T>(actual: unknown, expected: T, message?: string | Error): asserts actual is T;
  export function notEqual(actual: unknown, expected: unknown, message?: string | Error): void;
  export function deepEqual<T>(actual: unknown, expected: T, message?: string | Error): asserts actual is T;
  export function deepStrictEqual<T>(actual: unknown, expected: T, message?: string | Error): asserts actual is T;
  export function match(value: string, regexp: RegExp, message?: string | Error): void;
  export function throws(fn: () => unknown, error?: unknown, message?: string | Error): void;
  export function rejects(fn: (() => Promise<unknown>) | Promise<unknown>, error?: unknown, message?: string | Error): Promise<void>;

  const assert: {
    ok: typeof ok;
    equal: typeof equal;
    strictEqual: typeof strictEqual;
    notEqual: typeof notEqual;
    deepEqual: typeof deepEqual;
    deepStrictEqual: typeof deepStrictEqual;
    match: typeof match;
    throws: typeof throws;
    rejects: typeof rejects;
  };
  export default assert;
}
