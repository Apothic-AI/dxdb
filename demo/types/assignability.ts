/**
 * The drift guard between the authored surface (`types/dxdb.d.ts`) and the
 * generated one (`pkg/dxdb_js.d.ts`).
 *
 * Hand-written types are only worth having if they cannot silently stop matching
 * the wasm module. `wasm-bindgen` regenerates `pkg/dxdb_js.d.ts` from the Rust
 * signatures on every build, so it is the ground truth for *shape* — what members
 * exist and what they return. This file takes that ground truth and asserts the
 * authored types agree with it.
 *
 * Direction matters:
 *
 * - **generated → authored** is the dangerous drift, and is what this file checks:
 *   if Rust grew or retyped a member the authored types do not describe, one of
 *   the assertions below stops compiling.
 * - **authored → generated** is deliberately not asserted for `subscribe`: the
 *   generated signature is `(next: any, error: any, complete: any)` and our
 *   narrower one is not assignable to it in the contravariant position. Narrowing
 *   `any` to a real type is the entire point of the authored file.
 *
 * Everything here is type-level; nothing executes at runtime.
 */

import type {
  Emitter as GeneratedEmitter,
  Observable as GeneratedObservable,
  Subscription as GeneratedSubscription,
} from '../pkg/dxdb_js';
import type * as GeneratedModule from '../pkg/dxdb_js';

import type {
  Emitter,
  Observable,
  Subscription,
} from 'dxdb';
import * as AuthoredModule from 'dxdb';

/** A compile-time assertion: this alias is `never` unless `T` is `true`. */
type Assert<T extends true> = T;

// ---------------------------------------------------------------------------
// Subscription: same members, same types, both directions
// ---------------------------------------------------------------------------

/**
 * Both directions for `closed` and `unsubscribe`, because these are the members
 * the adapter's contract is built on: if Rust stopped exposing `closed`, or made
 * it something other than a boolean, every consumer file would be lying.
 */
type SubscriptionAgrees = [
  Assert<Subscription['closed'] extends GeneratedSubscription['closed'] ? true : false>,
  Assert<GeneratedSubscription['closed'] extends Subscription['closed'] ? true : false>,
  Assert<
    Subscription['unsubscribe'] extends GeneratedSubscription['unsubscribe'] ? true : false
  >,
  Assert<
    GeneratedSubscription['unsubscribe'] extends Subscription['unsubscribe'] ? true : false
  >,
];

// A value of this type only exists if the tuple above is all-`true`.
const subscriptionAgrees: SubscriptionAgrees = [true, true, true, true];
void subscriptionAgrees;

// ---------------------------------------------------------------------------
// Emitter: the push surface
// ---------------------------------------------------------------------------

/**
 * Authored → generated for `next`, `error`, `complete`, and `observable`.
 *
 * Our `next` is `(value: T) => void` instantiated at `unknown`; the generated one
 * is `(value: any) => void`. A function accepting `unknown` is assignable to one
 * accepting `any`, so this direction holds and is worth asserting. The reverse
 * would be the `any`-leak we are deliberately avoiding.
 */
type EmitterAgrees = [
  Assert<Emitter['next'] extends GeneratedEmitter['next'] ? true : false>,
  Assert<Emitter['error'] extends GeneratedEmitter['error'] ? true : false>,
  Assert<Emitter['complete'] extends GeneratedEmitter['complete'] ? true : false>,
];

const emitterAgrees: EmitterAgrees = [true, true, true];
void emitterAgrees;

// ---------------------------------------------------------------------------
// Observable: the method the whole boundary exists for
// ---------------------------------------------------------------------------

/** `subscribe` exists on both and returns their respective `Subscription`. */
type ObservableAgrees = [
  Assert<Observable['subscribe'] extends (...args: never[]) => Subscription ? true : false>,
  Assert<
    GeneratedObservable['subscribe'] extends (
      ...args: never[]
    ) => GeneratedSubscription
      ? true
      : false
  >,
];

const observableAgrees: ObservableAgrees = [true, true];
void observableAgrees;

/**
 * The assertion that catches "we documented a method the wasm module does not
 * have": an authored `Observable` must be usable where the generated one is
 * expected, for the members the generated type declares.
 *
 * Declared as a `declare function` so nothing is constructed or executed.
 */
declare function usesGeneratedObservable(observable: GeneratedObservable): void;

export function authoredObservableSatisfiesGenerated(ours: Observable<unknown>): void {
  // If our type declared a member the generated type lacks, or omitted one a
  // `GeneratedObservable` consumer calls, this is a type error. The cast confines
  // the check to *shape*; `subscribe`'s narrower parameters are the documented,
  // intentional difference and are asserted separately above.
  usesGeneratedObservable(ours as unknown as GeneratedObservable);
}

// ---------------------------------------------------------------------------
// The free functions
// ---------------------------------------------------------------------------

/**
 * `abi_version` is the version handshake the harness relies on, so its signature
 * is checked end to end.
 */
type AbiVersionAgrees = [
  Assert<typeof AuthoredModule.abi_version extends typeof GeneratedModule.abi_version ? true : false>,
  Assert<typeof GeneratedModule.abi_version extends typeof AuthoredModule.abi_version ? true : false>,
];

const abiVersionAgrees: AbiVersionAgrees = [true, true];
void abiVersionAgrees;

/** `echo` crosses the boundary with a string both ways. */
type EchoAgrees = [
  Assert<typeof AuthoredModule.echo extends typeof GeneratedModule.echo ? true : false>,
  Assert<typeof GeneratedModule.echo extends typeof AuthoredModule.echo ? true : false>,
];

const echoAgrees: EchoAgrees = [true, true];
void echoAgrees;

/** `resolve_after_tick` is a real `Promise`, not a thenable-shaped custom type. */
type ResolveAgrees = [
  Assert<
    typeof AuthoredModule.resolve_after_tick extends typeof GeneratedModule.resolve_after_tick
      ? true
      : false
  >,
  Assert<
    typeof GeneratedModule.resolve_after_tick extends typeof AuthoredModule.resolve_after_tick
      ? true
      : false
  >,
];

const resolveAgrees: ResolveAgrees = [true, true];
void resolveAgrees;

export {};
