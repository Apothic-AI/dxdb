/**
 * The compile check for the authored surface in `types/dxdb.d.ts`.
 *
 * This file is never executed. It exists to be type-checked, because a `.d.ts`
 * nobody compiles is a `.d.ts` that is wrong. Every declaration is consumed here,
 * and every `@ts-expect-error` is a *negative* assertion: if the compiler stops
 * rejecting that line, the error "unused @ts-expect-error" is itself the failure.
 *
 * Compile with `npm run typecheck`.
 */

import {
  Emitter,
  type CompleteFn,
  type DatabaseOptions,
  type ErrorFn,
  type HashFunction,
  type NextFn,
  type Observable,
  type Observer,
  type RemoteMode,
  type StorageName,
  type StorageOptions,
  type Subscription,
} from 'dxdb';

// ---------------------------------------------------------------------------
// The Observable adapter: both call shapes, and the generic carrying through
// ---------------------------------------------------------------------------

/** Values are typed when the observable is parameterised. */
const numbers: Observable<number> = new Emitter<number>().observable();

const sub1: Subscription = numbers.subscribe((value: number) => {
  // `value` must be `number`, not `unknown` and not `any`. If it were `any`, the
  // assignment below would compile and this check would be worthless.
  const doubled: number = value * 2;
  void doubled;
});
sub1.unsubscribe();
sub1.unsubscribe(); // idempotent, allowed
const closedFlag: boolean = sub1.closed;

const observer: Observer<number> = {
  next: (value) => void (value + 1),
  error: (err) => void err,
  complete: () => {},
};
const sub2: Subscription = numbers.subscribe(observer);

numbers.subscribe((v) => void v, (e) => void e, () => {});
numbers.subscribe({ next: (v) => void v });
numbers.subscribe({}); // legal: every member is optional
const sub3: Subscription = numbers.subscribe(() => {}, undefined, undefined);

// Explicitly-typed callback parameters are accepted too, which is what an app
// with strict settings written by hand will do.
const nextFn: NextFn<number> = (value: number) => void value;
const errorFn: ErrorFn = (err) => void err;
const completeFn: CompleteFn = () => {};
numbers.subscribe(nextFn, errorFn, completeFn);

void observer;
void sub2;
void sub3;
void closedFlag;

// The unparameterised form degrades to `unknown`, not `any`: this compiles...
const untyped = new Emitter().observable();
untyped.subscribe((value) => {
  // ...and here `value` is `unknown`, so this must be narrowed before use.
  if (typeof value === 'string') {
    const length: number = value.length;
    void length;
  }
});

// A generic emitter round-trips its type through `observable()`.
interface Doc {
  id: string;
  _deleted: boolean;
}
const docEmitter = new Emitter<Doc>();
const docs: Observable<Doc> = docEmitter.observable();
docs.subscribe((doc: Doc) => {
  const id: string = doc.id;
  void id;
});
docEmitter.next({ id: 'a', _deleted: false });

// ---------------------------------------------------------------------------
// Error paths: the type system must reject misuse
// ---------------------------------------------------------------------------

// @ts-expect-error `subscribe` requires at least the values callback.
numbers.subscribe();

// @ts-expect-error a number is not an observer and not a callback.
numbers.subscribe(42);

// @ts-expect-error the observer object's `next` receives the value type.
numbers.subscribe({ next: (value: string) => void value });

// @ts-expect-error `closed` is readonly.
sub1.closed = true;

// @ts-expect-error an `Observable` interface is not constructible; a producer is
// what creates one.
const impossible: Observable<number> = new Observable<number>();
void impossible;

// @ts-expect-error `unsubscribe` takes no arguments.
sub1.unsubscribe(true);

// ---------------------------------------------------------------------------
// The declared storage surface (types only; the functions are not exported yet)
// ---------------------------------------------------------------------------

const memoryOptions: StorageOptions = { storage: 'memory' };

const indexedDbOptions: StorageOptions = {
  storage: 'indexeddb',
  batchSize: 300,
};

const opfsOptions: StorageOptions = { storage: 'opfs', variant: 'worker' };

const localStorageOptions: StorageOptions = { storage: 'localstorage' };

const remoteModes: RemoteMode[] = ['one', 'storage', 'database', 'collection'];
const remoteOptions: StorageOptions = {
  storage: 'remote',
  mode: 'database',
  identifier: 'my-worker',
};

const hash: HashFunction = 'sha256';
const storageNames: StorageName[] = [
  'memory',
  'indexeddb',
  'opfs',
  'localstorage',
  'remote',
];

const dbOptions: DatabaseOptions = {
  name: 'exampledb',
  storage: 'opfs',
  storageOptions: opfsOptions,
  multiInstance: true,
  hashFunction: hash,
};

// The discriminated union must reject a mismatched option object: an
// `indexeddb` storage with OPFS options is a real bug class, and the union is
// what makes it a compile error.
// @ts-expect-error `variant` is not a member of the indexeddb options.
const mismatched: StorageOptions = { storage: 'indexeddb', variant: 'worker' };

// @ts-expect-error `batchSize` belongs to indexeddb, not memory.
const wrongStorage: StorageOptions = { storage: 'memory', batchSize: 300 };

// @ts-expect-error an unknown storage name is not assignable.
const unknownStorage: StorageName = 'sqlite';

// @ts-expect-error the remote option requires an identifier.
const remoteWithoutId: StorageOptions = { storage: 'remote' };

void memoryOptions;
void indexedDbOptions;
void localStorageOptions;
void remoteModes;
void remoteOptions;
void storageNames;
void dbOptions;
void mismatched;
void wrongStorage;
void unknownStorage;
void remoteWithoutId;

export {};
