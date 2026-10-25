/**
 * The authored TypeScript surface for `dxdb-js`, as it exists today.
 *
 * ## Why this file exists next to the generated `.d.ts`
 *
 * `wasm-bindgen` generates `pkg/dxdb_js.d.ts` from the Rust signatures, and for
 * `JsValue` parameters it emits `any`. That is honest about the wasm ABI and
 * wrong about the API: `subscribe(next: any, error: any, complete: any)` accepts
 * `subscribe(42)` and compiles. PLAN.md 5.1 asks for "a hand-written `.d.ts` a
 * real app can compile against. No `any` in the public surface", so the authored
 * types live here and the generated file stays a build artifact.
 *
 * The obvious risk of hand-writing types is drift: this file could describe a
 * surface the wasm module does not have. Two things guard against that, and they
 * are both in this directory:
 *
 * - `types/consumer.ts` is compiled with `tsc --noEmit`, so every declaration
 *   here is exercised. A declaration nothing consumes is a declaration nobody has
 *   checked.
 * - `types/assignability.ts` asserts this surface and the *generated* one are
 *   mutually assignable for the members they share, so a Rust signature change
 *   that this file has not followed becomes a compile error rather than a
 *   runtime surprise.
 *
 * ## What "no `any`" means here
 *
 * The public surface has no `any`. The one unavoidable boundary is the value a
 * stream carries: wasm-bindgen hands the adapter a `JsValue`, whose TypeScript
 * representation is `any`. That is expressed as a generic default of `unknown`
 * (`Observable<T = unknown>`) so a caller either parameterises or is forced to
 * narrow — `unknown` will not silently flow into a typed variable, which is
 * exactly the property `any` lacks.
 */

declare module 'dxdb' {
  /** A value that crossed the wasm boundary. `unknown` is deliberate: see above. */
  export type WasmValue = unknown;

  /**
   * A callback delivered once per value.
   *
   * Generic in the value type so an app declares `Observable<Doc>` and the
   * callback is typed. The untyped default is `unknown`, never `any`.
   */
  export type NextFn<T = WasmValue> = (value: T) => void;

  /** A callback delivered at most once, on the error path. */
  export type ErrorFn = (error: Error | WasmValue) => void;

  /** A callback delivered at most once, on the success path. */
  export type CompleteFn = () => void;

  /**
   * The observer-object form of `subscribe`.
   *
   * All three members are optional: a consumer that only wants values passes
   * `{ next }` and the terminal events are no-ops. That is the same tolerance the
   * adapter implements in Rust (`parse_callbacks`), and it is what makes
   * `subscribe({})` legal rather than a type error.
   */
  export interface Observer<T = WasmValue> {
    next?: NextFn<T>;
    error?: ErrorFn;
    complete?: CompleteFn;
  }

  /**
   * What `subscribe` returns.
   *
   * `unsubscribe()` is idempotent, and after it returns **no callback fires** —
   * including one the producer had already queued. `closed` reports whether
   * *this* consumer has unsubscribed; it is not a terminal-event indicator. A
   * consumer that needs to know the stream ended uses the `complete` callback.
   */
  export interface Subscription {
    /** Stop delivery. Idempotent; safe to call from inside a callback. */
    unsubscribe(): void;
    /** `true` once this consumer has unsubscribed. */
    readonly closed: boolean;
  }

  /**
   * A stream of values.
   *
   * Structurally compatible with what an RxJS `from()` accepts, so RxJS works
   * over it without `dxdb-js` depending on RxJS (PLAN.md 3.3).
   *
   * The two call shapes are both legal, because the adapter implements both:
   *
   * ```ts
   * obs.subscribe((v) => console.log(v));
   * obs.subscribe({ next: (v) => console.log(v), complete: () => {} });
   * obs.subscribe((v) => {}, (err) => {}, () => {});
   * ```
   *
   * `Observable` cannot be constructed directly; it comes from an `Emitter` (or,
   * once the storages land, from a collection's `changes`/`find$`). That is why
   * there is no constructor in this interface: a `new Observable()` would have no
   * producer and would never emit.
   */
  export interface Observable<T = WasmValue> {
    /**
     * Subscribe with callbacks or an observer object.
     *
     * The single-signature form is used rather than three overloads because the
     * adapter resolves the shape at runtime and an overload set would have to be
     * kept in sync with `parse_callbacks` by hand. It types the *arguments*, which
     * is the part that catches bugs.
     */
    subscribe(
      nextOrObserver: NextFn<T> | Observer<T>,
      error?: ErrorFn,
      complete?: CompleteFn,
    ): Subscription;
  }

  /**
   * The producer side of an `Observable`, pushed to by hand.
   *
   * Not the long-term public surface — real streams will come from storages — but
   * it is the source a non-Rust producer uses (a message channel, a DOM event) and
   * what the harness tests drive.
   *
   * Delivery is **synchronous**: `next()` calls the subscriber's callback before it
   * returns, the same semantics as an RxJS `Subject`. A value emitted from inside
   * a callback is therefore delivered from inside that callback.
   */
  export interface Emitter<T = WasmValue> {
    /** Emit a value. Dropped silently when nothing is subscribed. */
    next(value: T): void;
    /** End the stream. `complete` fires at most once, never after an error. */
    complete(): void;
    /** Fail the stream. `error` fires at most once, never after `complete`. */
    error(message: Error | WasmValue): void;
    /** The `Observable` half of this stream. */
    observable(): Observable<T>;
  }

  /** The constructor for {@link Emitter}, as the wasm module exports it. */
  export const Emitter: {
    new <T = WasmValue>(): Emitter<T>;
  };

  /**
   * The schema hash function currently supported.
   *
   * A single-member union rather than `string`: it is what the implementation
   * accepts today, and widening it later is not a breaking change for callers,
   * whereas narrowing a `string` would be.
   */
  export type HashFunction = 'sha256';

  /**
   * Options for {@link createDatabase}.
   *
   * Present as the declared shape of the surface this crate is building toward
   * (`docs/lanes/m4-browser-plan.md` §2.1) but `createDatabase` is **not exported
   * yet**, because the collection/storage layer it needs is gated on the storage
   * traits landing in `dxdb-js`. Declaring the type now lets the consumer file
   * prove the shape is expressible; it does not claim the function exists.
   */
  export interface DatabaseOptions {
    name: string;
    storage: StorageName;
    storageOptions?: StorageOptions;
    multiInstance?: boolean;
    ignoreDuplicate?: boolean;
    hashFunction?: HashFunction;
  }

  /** The storages this build can name. */
  export type StorageName =
    | 'memory'
    | 'indexeddb'
    | 'opfs'
    | 'localstorage'
    | 'remote';

  /**
   * Storage-specific options, discriminated on the storage name.
   *
   * `opfs` is split into the worker and main-thread variants because they are
   * different implementations with different cost curves, not a flag — see
   * PLAN.md 5.4 and `docs/lanes/m4-browser-plan.md` §5.
   */
  export type StorageOptions =
    | { storage: 'memory' }
    | { storage: 'indexeddb'; batchSize?: number; indexedDB?: IDBFactory }
    | { storage: 'opfs'; variant: 'worker' | 'main-thread' }
    | { storage: 'localstorage'; localStorage?: Storage }
    | { storage: 'remote'; mode?: RemoteMode; identifier: string };

  /** How a remote storage multiplexes its message channel. */
  export type RemoteMode = 'one' | 'storage' | 'database' | 'collection';

  /**
   * The `wasm-bindgen` ABI version string this module was built with.
   *
   * Used by the harness to prove the `.wasm` and the glue it loaded are the same
   * build.
   */
  export function abi_version(): string;

  /** Round-trip a string through the wasm boundary. */
  export function echo(input: string): string;

  /**
   * Resolve after one microtask with `value + 1`.
   *
   * Proves `wasm-bindgen-futures` links and that a Rust `Future` becomes a JS
   * `Promise` — the mechanism every async storage operation uses.
   */
  export function resolve_after_tick(value: number): Promise<number>;
}
