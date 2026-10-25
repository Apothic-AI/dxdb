# `dxdb-encryption`: behavioural specification

| | |
| --- | --- |
| Stage | B2 (CLEANROOM.md section 7) |
| Crate | `crates/cleanroom/dxdb-encryption` |
| Documented upstream module | `encryption-web-crypto` (CLEANROOM.md section 4 row 6) |
| Spec author read | `docs-src/docs/encryption.md`; the Apache-2.0 `src/plugins/encryption-crypto-js/index.ts`; the Apache-2.0 schema-hash and revision helpers; RFC 9106, RFC 8439, draft-irtf-cfrg-xchacha-03, NIST SP 800-63B, OWASP Password Storage; RustCrypto crate docs |
| Spec author did not read | `rxdb-premium` in any form (source, bundles, `.d.ts`, source maps, installer, payloads) |
| Full provenance | [`READING-LIST.md`](./READING-LIST.md), row `dxdb-encryption` |
| Gap rows this spec depends on | G-001, G-002, G-003, G-006, G-030, G-031, G-032, G-033, G-034, G-035, G-036 |
| Status | spec ready for review; implementation gated on `dxdb-core` |

## 0. Scope, and the one sentence that matters most

This crate encrypts the fields a schema marks as encrypted, in a storage wrapper.
It does not encrypt the whole database, does not encrypt documents as opaque blobs
(some fields must stay queryable), and does not attempt to hide access patterns,
document counts, field names outside the encrypted set, or timestamps.

The honest compatibility statement, stated here and repeated in the crate docs
because CLEANROOM.md section 3 requires it plainly:

> **dxdb's envelope is not compatible with WebCrypto, with the Apache-2.0
> `crypto-js` plugin, or with the paid WebCrypto plugin, and cannot be made
> compatible with the latter two.** A field encrypted by any upstream plugin
> cannot be decrypted by `dxdb-encryption`, and vice versa. The supported migration
> path is the portable one: read documents with the old system in plaintext, write
> them with dxdb.

There are two independent reasons, and it is worth separating them:

1. **The paid plugin's envelope is unknown.** No public specification of its byte
   layout, KDF parameters, or AAD construction exists. G-001 applies, G-002
   applies. Nothing can be done about this without a published format from the
   vendor.
2. **The open plugin's envelope is broken, and we will not reproduce it.** The
   Apache-2.0 `crypto-js` plugin calls `AES.encrypt(value, password)` directly:
   CryptoJS's password-based path derives a key with OpenSSL's `EVP_BytesToKey`
   (one MD5 iteration, no salt unless a random salt is supplied, which it is here),
   and the ciphertext carries no version, no algorithm identifier, no parameters,
   and no associated data. Reproducing that would mean reproducing an envelope with
   a fast, unsalted-iteration KDF and no binding between ciphertext and document.
   Compatibility with it is possible in principle and refused on purpose. See G-030.

So: dxdb defines its own documented envelope. That is a feature with a cost, and
section 6 lists what the cost is.

## 1. Observable behaviour

### 1.1 Per-field, schema-declared, transparent

> "To define a field as being encrypted, you have to add it to the `encrypted`
> fields list in the schema." — `docs-src/docs/encryption.md`

> "RxDB handles the encryption and decryption of data internally. This means that
> when you work with a `RxDocument`, you can access the properties of the document
> just like you would with normal, unencrypted data."

Observable: a field listed in the schema's `encrypted` array is stored as a string
and returned as its original JSON value. A field not listed is stored as-is. The
caller of `find_documents_by_id`, `query`, `bulk_write` and the attachment reader
cannot tell, from the value, whether it was encrypted on the way in — only from the
bytes in the underlying storage.

Encryption is applied at the storage-instance boundary: the wrapper transforms on
write and on read. Everything above it sees plaintext, everything below it sees
ciphertext.

### 1.2 A parent path is encrypted whole; a child path of an encrypted parent is rejected

> "No. When you encrypt a parent field, the entire object at that path is encrypted
> as a single string. You cannot also encrypt a child path of an already-encrypted
> parent. For example, if you encrypt `assignee`, you must **not** also add
> `assignee.email` to the `encrypted` array. Doing so will throw an error in
> dev-mode."

Observable: `encrypted: ["assignee"]` stores `assignee` as one ciphertext of the
whole object. `encrypted: ["assignee", "assignee.email"]` is a schema error at
database open, not a runtime surprise on first write. dxdb makes this a hard error
in every build, not only in dev mode: silently double-encrypting produces data that
cannot be read back, and a check that runs only in dev mode ships that data.

### 1.3 Encrypted fields cannot be used in queries

> "**Encrypted fields cannot be used as operators in queries**. This means you
> cannot perform queries like 'find all documents where the encrypted field equals
> a certain value.'"

Observable: a selector that constrains an encrypted path returns no matches (it is
matching against ciphertext) rather than erroring, *unless* dxdb can detect the
attempt, in which case it errors. dxdb's decision: **reject at query preparation**.
A query on an encrypted path is a caller bug that is otherwise invisible until a
user notices their search box silently returns nothing. This is a divergence from
upstream's behaviour, which passes the ciphertext into the matcher; recorded in
G-031.

A sort on an encrypted path is likewise rejected: sorting ciphertext is sorting
random bytes, so the order is not the order of anything the caller can name.

### 1.4 Attachments

> "To store the attachments data encrypted, you have to set `encrypted: true` in
> the `attachments` property of the schema."

Observable: with `attachments.encrypted: true`, attachment bytes are encrypted with
the same key and the same envelope, and the wrapper's attachment reader returns the
original bytes. With it `false` or absent, attachments are stored as-is even when
document fields are encrypted. That distinction is upstream's, and it is preserved:
encrypting attachments is a per-collection opt-in, because attachment bytes are
often large and the caller may already have encrypted them.

### 1.5 The password, and what happens when it is wrong

> "RxDB does not define how you should store or retrieve the encryption password."

> "Opening an existing database with a different password will throw an error."

Observable: a wrong password fails, loudly, at a **definite** point, and it fails as
a distinct error kind rather than as a parse error somewhere downstream. dxdb
detects it at database open rather than at first-read-of-some-field, because a
password check that happens lazily means a user can work for minutes before their
data "disappears". See section 3.4 for the check itself.

There is also a documented minimum length upstream (`MINIMUM_PASSWORD_LENGTH = 8`,
`src/plugins/encryption-crypto-js/index.ts:32`). dxdb **rejects** that specific
number: eight characters is below every current password-strength guideline and
gives a false sense of a security floor. dxdb uses entropy-based strength
reporting (section 3.3) and refuses only passwords that are trivially guessable.
G-032.

### 1.6 The password cannot be changed in place

> "The password is set database specific and it is not possible to change the
> password of a database. Opening an existing database with a different password
> will throw an error. To change the password you can either: use the storage
> migration plugin … or store a randomly created meta-password in a different
> RxDatabase …"

Observable: there is no `change_password`. There is, however, **key rotation**
(section 3.6), which is a different operation with a different guarantee: rotation
re-encrypts every field of every document from key generation `n` to generation
`n+1` under the same password, so a stolen *derived key* stops working while the
user's password stays the same. Upstream has no such path at all — its answer is
"migrate to a new database". Rotation is an addition, recorded in G-033.

### 1.7 What encryption does not hide

Worth stating because it is the most common misunderstanding, and CLEANROOM.md
section 3 requires documented differences to be visible:

- The **document schema** is not encrypted. Field names, types and the `encrypted`
  list are readable in storage.
- **Document ids, revisions, `_deleted`, and `_meta.lwt` are not encrypted.** They
  are queryable by design; a query on a primary key must still work.
- A field's **ciphertext length** leaks the approximate plaintext length (see
  section 4, "length disclosure").
- **Whether a document has an encrypted field set** is visible: an absent field is
  absent, a present one is a string of ciphertext.
- **Counts, access patterns and timing** are not hidden.

## 2. Public API shape (from docs)

The documented JavaScript shape, and dxdb's mapping. Every documented option appears
exactly once; nothing is added that the documentation does not name, and nothing named
in the documentation is dropped.

```ts
import { wrappedKeyEncryptionWebCryptoStorage, createPassword }
    from 'rxdb-premium/plugins/encryption-web-crypto';

const encryptedStorage = wrappedKeyEncryptionWebCryptoStorage({
    storage: getRxStorageIndexedDB({})
});

const database = await createRxDatabase({
    name: 'mydatabase',
    storage: encryptedStorage,
    password: { algorithm: 'AES-CTR', password: 'myRandomPasswordWithMin8Length' }
});
```

| Documented | dxdb | Notes |
| --- | --- | --- |
| `wrappedKeyEncryptionWebCryptoStorage({ storage })` | `EncryptionStorage::new(inner, config)` | wrapper constructor, same position in the stack |
| `createRxDatabase({ password })` — a string | `Password::from_str(..)` | the password is not stored on the storage wrapper; it reaches it through instance creation, as upstream does |
| `password: { algorithm, password }` — an object | **not modelled** | the algorithm choice is not exposed (G-034); accepting an algorithm the envelope does not honour would be worse than not accepting it |
| `algorithm: 'AES-CTR' \| 'AES-CBC' \| 'AES-GCM'` | n/a | two of those three are not AEAD; we ship one AEAD cipher and no choice (G-034) |
| `schema.encrypted: string[]` | `EncryptedPaths` (validated at open) | same list, with the nesting rule enforced in every build (1.2) |
| `schema.attachments.encrypted: bool` | `attachments_encrypted: bool` | per-collection opt-in, preserved |
| "not possible to change the password" | no `change_password`; `rotate()` instead | rotation changes the key, not the password (3.6, G-033) |
| `removeRxDatabase()` requires the password | `remove()` requires an unlocked instance | matches: you cannot delete what you cannot open |
| `MINIMUM_PASSWORD_LENGTH = 8` (`encryption-crypto-js/index.ts:32`) | **not adopted** | entropy-based floor instead (G-032) |

Types:

```rust
pub struct EncryptionConfig { pub encrypted: EncryptedPaths, pub attachments_encrypted: bool }
pub struct EncryptedPaths(Vec<String>);           // validated: no child of an encrypted parent
pub struct Password(Zeroizing<String>);           // never Debug, never Display, never logged
pub struct Envelope { /* 47-byte header + body, see section 4 */ }
pub struct KeyGeneration(u8);
pub enum EncryptionError { WrongPassword, PasswordRequired, PasswordTooWeak,
                           UnsupportedEnvelope { version: u8, suite: u8 },
                           AuthenticationFailed, MalformedEnvelope { reason: String },
                           EncryptedPathInQuery, EncryptionInWorkerRequired }
```

The error list is closed: eight variants, and section 5 enumerates when each occurs.

## 3. Design decisions

Each decision below states the choice, the numbers, and the reason. Where the
reason is a documented threat model, the threat model is named.

### 3.1 The threat model, stated first, because the KDF parameters follow from it

Encrypting a local-first database is not the same problem as hashing a server-side
password. The adversary we design against:

| # | Threat | In scope | Consequence for the design |
| --- | --- | --- | --- |
| T1 | Stolen device or copied storage files, offline analysis | **yes** | KDF must be memory-hard so that guessing a password against a copied file is expensive |
| T2 | Another application on the same device reading the data files | **yes** | field ciphertext must be useless without the password, and the key must live in process memory only |
| T3 | Ciphertext transplant: moving a field's ciphertext into another document to forge a value | **yes** | AAD must bind the ciphertext to its document and to the schema |
| T4 | Rollback: replaying an older ciphertext for the same field to undo a newer value | **yes** | AAD must bind to something that changes every write; see 3.4 |
| T5 | Active attacker with the password guessing online, at interactive speed | **partly** | KDF cost bounds their rate; the in-memory key cache bounds our own unlock cost |
| T6 | Memory scraping of a live process by a same-user attacker | **no** | Not defended. Any same-user process can read our memory; a local-first database cannot pretend otherwise |
| T7 | Traffic analysis, access patterns, document counts | **no** | Out of scope; stated in 1.7 |
| T8 | An attacker who has the password | **no** | If they have the password they have the database. Nothing here changes that |

The parameters in 3.3 are chosen for T1 and T4, which is why they are tuned for
*offline guess cost* rather than for "short unlock time".

### 3.2 Cipher: XChaCha20-Poly1305, 256-bit key, 128-bit tag

- **AEAD**: XChaCha20-Poly1305 per draft-irtf-cfrg-xchacha-03 (HChaCha20 subkey from
  the first 16 nonce bytes, then RFC 8439 AEAD_CHACHA20_POLY1305 with the remaining
  8 bytes). 32-byte key, 24-byte nonce, 16-byte tag.
- **Why XChaCha over AES-GCM**: the 192-bit nonce makes a random-nonce strategy safe
  by a wide margin (`2^80` messages per key before collision probability is
  interesting, versus `2^32` for a 96-bit nonce under a naive random strategy), so we
  do not need a per-key counter that would have to be persisted and synchronised
  across tabs and processes. The port explicitly forbids hidden shared state between
  instances (PLAN.md 4.2's wrapper pattern), so a counter would be the wrong shape.
- **Why not `aes-gcm`**: two reasons. AES-GCM is the cipher with the worse
  nonce-reuse failure mode (reuse under AES-GCM is catastrophic and undetectable in
  software), and AES-GCM's performance advantage needs AES-NI, which is absent on
  some of our targets while ChaCha20 is fast in portable code. CLEANROOM.md 5.5
  allows `aes-gcm` "as an option when running in a browser, chosen by benchmark, not
  by habit". Decision: **no AES option in v1**. Adding a second cipher doubles the
  envelope's test surface and the number of ways to produce unreadable data, for a
  benchmark we have not run. G-034 records this and what would reopen it.
- **Constant-time, no branching on secret data**: provided by the RustCrypto
  implementation; we do not implement any primitive ourselves.

### 3.3 KDF: Argon2id, m = 64 MiB, t = 3, p = 1, 32-byte output

The parameters, their source, and why not the others:

| Parameter | Value | Source / reason |
| --- | --- | --- |
| Variant | Argon2id | RFC 9106 §4 step 3: "if you do not know the difference between the types or you consider side-channel attacks to be a viable threat, choose Argon2id" |
| Memory `m` | 65 536 KiB (64 MiB) | RFC 9106 §4 SECOND RECOMMENDED uses `m = 2^16`; see the budget argument below |
| Iterations `t` | 3 | RFC 9106 §4 SECOND RECOMMENDED pairs `t = 3` with `m = 2^16` |
| Lanes `p` | 1 | See "Why p = 1" below |
| Salt | 16 bytes, per database, random | RFC 9106 §3.1: "16 bytes is RECOMMENDED for password hashing" |
| Tag length | 32 bytes (256-bit key) | RFC 9106 §4 step 8: 128 bits suffices for key derivation, we take 256 to match the cipher key |
| Version | 0x13 (v1.3) | RFC 9106 §3.1 |

**Why the SECOND RECOMMENDED option and not the FIRST.** RFC 9106 §4 offers
`t=1, p=4, m=2 GiB` first, and `t=3, p=4, m=64 MiB` second "if much less memory is
available". 2 GiB per unlock is not available on our targets: this is a browser
database that must unlock on a mid-range phone, inside a tab, next to the
application's own heap. A WebAssembly linear memory cannot allocate 2 GiB on a
32-bit wasm memory, so the FIRST option is not merely expensive here, it is
unreachable on one of our primary targets. Choosing between "the recommendation we
cannot honour" and "the recommendation designed for exactly this constraint", we
take the second — and we take it as written, including its `t = 3`, rather than
mixing its memory with the first option's iteration count.

Note that this is *the RFC's own answer to our constraint*, not a weakening we
invented. That distinction is the whole point of citing it.

**Why p = 1, when both RFC options say p = 4.** Two reasons, both specific:

1. On `wasm32-unknown-unknown` **without** the threads feature — which is the
   default build and the one the workspace gate compiles (PLAN.md 3.4: threads need
   either `wasm32-wasip1-threads` or `-Z build-std`, so they are an opt-in
   configuration, never the default) — there is exactly one thread. `p = 4` against
   a single-threaded runtime is `p = 1` with three idle lanes: identical security,
   identical memory, no speedup.
2. Argon2's memory is `m` KiB regardless of `p`, but `m` must be at least `8p`
   (RFC 9106 §3.1). Keeping `p = 1` keeps the parameter set identical between the
   single-threaded and threaded builds, so a database unlocked on a phone is
   unlocked with the same work factor on a desktop. A build-dependent `p` would mean
   two derivations of the same key material from the same parameters, which is a bug
   waiting to be filed.

`p` is a *speed* parameter, not a strength parameter, once memory is fixed. G-035.

**Why not Argon2i.** RFC 9106 §1: Argon2i is for settings where side-channel
attacks are the dominant threat; our adversary is offline with a copied file (T1),
not co-resident (T6 is out of scope). Argon2id gives Argon2i's protection for the
first half-pass anyway.

**Why not the `argon2` crate's defaults.** The RustCrypto `argon2` crate's
`Params::default()` is `m = 19 456 KiB, t = 2, p = 1`, which is OWASP's *minimum*
recommendation. Adopting a library default is exactly the "by habit" that
CLEANROOM.md 5.5 warns against, and it is 3.4x less memory than the RFC's
low-memory recommendation. We state our numbers and validate them against the RFC.

**Why not scrypt or PBKDF2.** PBKDF2 is not memory-hard at all (no defence against
GPU/ASIC parallel search, T1). scrypt is acceptable but is not an RFC 9106
recommendation and its parameterisation (`N`, `r`, `p`) is harder to reason about
for our memory budget. Argon2id was the Password Hashing Competition winner and is
the RFC 9106 primary variant; there is no reason to pick a second-best option.

**The budget argument, in numbers.** Argon2id at `m = 64 MiB, t = 3, p = 1` costs
roughly 3 sequential passes over 64 MiB of memory. On a machine that sustains
~1 GB/s of Argon2 throughput per core, that is on the order of 200 ms — a one-time
unlock cost, not a per-operation cost (the derived key is cached; see 3.7). On a
phone it will be slower, which is fine for a one-time unlock and is the entire
point of a memory-hard KDF: the same 64 MiB makes an attacker's per-guess cost
identical to ours. **This is an estimate, not a measurement**, and the crate may not
describe its unlock cost as any specific number until section 8 has a measured
value.

### 3.4 AAD: document id + schema hash + field path + revision

The associated data binds each ciphertext to four things:

```
AAD = len-prefixed concat of:
  "dxdb.enc.v1"                      // domain separator, 11 bytes
  document_id                        // the document's primary key value
  schema_hash                        // 32 raw bytes, SHA-256
  field_path                         // the encrypted path, e.g. "assignee"
  revision                           // the document's _rev being written
```

Why each element, tied to a threat:

- **`document_id`** defeats T3 in its simplest form. Without it, an attacker with
  write access to storage copies the `note` ciphertext from document A into
  document B: B now shows A's note, and it decrypts cleanly, because the key is the
  same. With the id in the AAD, that ciphertext fails the Poly1305 tag check in B.
- **`schema_hash`** defeats T3 in its second form: moving a ciphertext between
  *collections*. Two collections in one database share a key, so the document id
  alone is not enough — the same id can exist in both. The schema hash is the
  collection's identity (`RxSchema.hash`, which upstream defines as
  `SHA-256(JSON.stringify(schema))` with a per-database `hashFunction`, and which
  the internal store persists as `schemaHash` next to each collection). Binding to
  it means a ciphertext is valid only under the schema that produced it, so a
  ciphertext cannot be transplanted into a collection with a different shape — and
  it fails closed if a schema migration changes the hash.
- **`field_path`** defeats a narrower transplant: moving a ciphertext from field
  `note` to field `secret` within the same document. Cheap to include; the failure
  mode without it is a field showing another field's plaintext.
- **`revision`** defeats T4, rollback. This is the element that needs the most
  justification, because it is not in CLEANROOM.md 5.5's list.

  Without a per-write component, an attacker can replace the current `note`
  ciphertext with the `note` ciphertext from an earlier revision of the *same*
  document. It decrypts, because the id and schema hash are unchanged. The user's
  note silently reverts to an old value. Adding the document's `_rev` to the AAD
  makes every write's ciphertext unique to that revision: replaying an older
  ciphertext produces a ciphertext whose AAD no longer matches, so the tag check
  fails and the read errors instead of silently returning stale data.

  This is exactly what the brief asks for ("non-transplantable between documents,
  and between a document and its own older revision"), and it costs nothing at read
  time because `_rev` is already on the document being read.

  The residual gap, stated rather than hidden: an attacker who can rewrite *both*
  the ciphertext and the `_rev` field consistently, and who knows the revision
  format, can still present an older `(ciphertext, revision)` pair as current.
  Defeating that needs a monotonic version the attacker cannot roll back — which is
  the meta cache's validity problem (specs/meta-cache.md), not the envelope's. See
  G-036.

**Encoding.** AAD is a length-prefixed concatenation, not a joined string: every
element is `u32` big-endian length followed by its bytes. This removes any
ambiguity from a document id that contains the separator, which a joined string
would introduce silently.

**Why the schema hash and not a per-collection salt.** A per-collection salt would
be another thing to store and to get wrong; the schema hash is already a unique,
stable, persisted per-collection value that the port computes anyway, and reusing
it avoids inventing a second identity for the same thing.

### 3.5 The per-database salt: where it lives, and why there

- **One salt per database**, 16 random bytes, generated on first creation.
- Stored in the database's **internal store**: the `_rxdb_internal` collection
  (`src/rx-storage-helper.ts:51`), under a storage-token document, alongside the
  password verifier (3.4's check). Upstream already stores a `passwordHash` in
  exactly this document (`src/rx-database-internal-store.ts:163,182`), so the salt
  has a natural, already-persisted home and needs no new collection.
- **Not per collection, not per document**: the salt's job is to make one password
  derive a different key in every database, so that precomputation against a
  popular password does not transfer between databases. Per-document salts would
  require a KDF call per document, which is 64 MiB and ~200 ms *per document* —
  absurd. Per-database is the only granularity at which a memory-hard KDF is
  affordable.
- **The salt is not secret.** It is stored in plaintext, by design; a salt defends
  against precomputation and rainbow tables, not against an attacker who already has
  the file.
- **The internal store itself is not encrypted.** It cannot be: it holds the salt
  and verifier needed to decrypt everything else, so encrypting it would be
  circular. Its contents are a schema hash, a collection name, a salt and a
  verifier; none of that is user data. This is stated in 1.7's spirit and is the
  standard bootstrap problem, but it deserves to be written down rather than
  discovered.

### 3.6 Key rotation: what rotates, what does not, and the guarantee

Rotation is a first-class, tested path. The shape:

1. A document field's ciphertext records its **key generation** in the envelope
   (`gen`, section 3.8). Generation 1 is created with the database.
2. Database-level `salt` and every generation's KDF salt live in the internal store.
3. `rotate()` derives generation `n+1` from the *same password* and a **new 16-byte
   KDF salt**, then rewrites every encrypted field of every document in the
   collection with generation `n+1`, in batches, with resumable progress.
4. Reads during rotation accept both generations, so the database stays usable.
   Writes during rotation use the new generation.
5. When every document is at `n+1`, the generation `n` salt is dropped from the
   internal store, and generation `n` ciphertext becomes undecryptable. That is the
   point of rotating.

The guarantee, and it is the one that gets tested: **for every document, the
plaintext after rotation equals the plaintext before rotation, byte for byte.**
Not "equivalent", not "the same fields": the same plaintext bytes.

Why rotate at all, given the password cannot change: rotation invalidates a
*leaked derived key* (T5's aftermath) without requiring the user to pick a new
password, and it is the operation you want to be able to run after a device is
lost. Upstream's only answer is a full database migration. G-033.

Interruption safety: rotation is resumable, and a document either has generation `n`
or `n+1`, never a mixture — a single document's encrypted fields are rewritten
atomically with respect to that document's write. Batches are per document, matching
`bulk_write`'s per-document atomicity (PLAN.md 4.2), so a crash mid-batch leaves
some documents on `n` and some on `n+1`, both readable.

### 3.7 Key lifecycle in memory

- The password-derived key is cached in **process memory only**, in a
  `Zeroizing<[u8; 32]>`, for the lifetime of the open database, and never written to
  disk, to a log, or to an error message.
- The KDF runs **once per open**, not once per operation. A per-operation derivation
  at 64 MiB and ~200 ms would make the wrapper unusable.
- Dropping the database handle zeroes the key. A best-effort `mlock` on native
  targets is a non-goal: it does not survive swapping in a browser and gives a false
  sense of protection (T6 is out of scope).

### 3.8 The envelope: byte layout

```text
ciphertext_field :=
    magic        4 bytes   "DXE1"            // 'D' 'X' 'E' '1'
    version      1 byte    0x01              // envelope version
    suite        1 byte    0x01              // 0x01 = Argon2id + XChaCha20-Poly1305
    gen          1 byte    key generation    // 1..=255
    nonce       24 bytes   random, per write
    tag         16 bytes   Poly1305
    body         N bytes   XChaCha20 keystream over the plaintext
```

- Total overhead: 4 + 1 + 1 + 1 + 24 + 16 = **47 bytes** per encrypted field.
- The field is stored as a **string**: the byte string above, base64-encoded with
  the standard alphabet and **no padding**. Fixed overhead in the stored
  representation is 47 bytes -> 63 base64 characters (ceil(47/3)*4 = 64 minus one
  padding char).
- The plaintext is the field's **JSON serialization** (`JSON.stringify` semantics,
  produced by `dxdb-json::to_js_json` so that number and string formatting is
  JS-identical). Not a bespoke encoding: it means the encrypted value round-trips
  through the same encoder as the rest of the document, which PLAN.md 3.1 requires
  anyway.
- Algorithm parameters are **not** in the envelope: they live once in the internal
  store per generation. Putting 64 MiB-worth of Argon2 parameters in every field
  would waste 40+ bytes per field to say the same thing 10 000 times, and it invites
  an attacker to rewrite them.

### 3.9 How a version is recognised, and how a future version is handled

- `magic` identifies the envelope family. Anything that is not `"DXE1"` is not a
  dxdb envelope.
- `version` identifies the layout within that family. `0x01` is this document.
- `suite` identifies the KDF+cipher pair. `0x01` is this document.
- **A field whose `version` or `suite` is unknown is a hard error**
  (`UnsupportedEnvelope`), never a partial parse and never "try the newest parser".
  A parser that guesses is a parser that silently accepts forged data.
- **Encrypter never writes a version it cannot read.** There is one writer, and it
  writes `0x01` until a `0x02` reader and writer ship together.
- Forward compatibility contract: a future version may reuse `"DXE1"` (layout
  change) or a new magic with a new family (algorithm change). Both are recognised
  and both produce `UnsupportedEnvelope` with the version byte in the error, so a
  user on an old build gets "this database was written by a newer dxdb" instead of
  "decryption failed".
- **Downgrade resistance for the version byte is not attempted.** An attacker who
  can rewrite storage can rewrite `0x02` to `0x01` and produce a confusing error;
  they cannot produce a successful decryption, because `version` is not in the AAD
  and the body's length and tag will not match. Recording this because "should the
  version be authenticated?" has a real answer: putting it in the AAD would mean a
  future version could never be read by an old build even theoretically, and it buys
  nothing against an attacker who already has write access.

### 3.10 Nonces

- 24 random bytes per field write, from the platform CSPRNG
  (`getrandom` on native and `wasm32-wasip1`, `crypto.getRandomValues` via a
  caller-supplied RNG on `wasm32-unknown-unknown`, which has no `getrandom` source).
- Random, not counter-based (3.2). With a 192-bit nonce, a collision needs ~2^96
  encryptions under one key; the birthday bound at 2^80 messages gives a collision
  probability below 2^-32, and no realistic database of fields approaches 2^80.
- **No nonce is ever reused for a given (key, AAD) pair under our control**, and a
  nonce is never derived from a counter, a timestamp, or an id, because all three
  are attacker-influenced or can repeat after a restore-from-backup. A restore that
  replays writes with counter-derived nonces would be a catastrophic nonce reuse;
  random nonces make restore-from-backup safe.


## 4. Invariants

Each names the mechanism that enforces it and the test that would fail without it.

**INV-1 — round-trip fidelity.** For any JSON value `v`, `decrypt(encrypt(v)) == v`
by value equality, including key order (PLAN.md 3.1 makes key order part of
document identity), JS number formatting, and non-ASCII strings.
*Enforced by* serialising through `dxdb-json::to_js_json` and not through
`serde_json`. *Proved by* `inv_01_round_trip_is_byte_identical`.

**INV-2 — ciphertext never appears in a plaintext position.** For every read path
(`find_documents_by_id`, `query`, `count`, attachments, `get_changed_documents_since`),
no returned value contains a base64 string parseable as a `DXE1` envelope.
*Enforced by* decryption being mandatory on the read path, with a missing
`decrypted` result being an error rather than a pass-through. *Proved by*
`inv_02_no_ciphertext_reaches_the_caller` across every operation the storage exposes.

**INV-3 — transplant between documents is rejected.** Moving a field's ciphertext
from document A to document B, keeping the field path and both documents' schemas
identical, fails authentication rather than returning A's plaintext under B.
*Enforced by* `document_id` in the AAD. *Proved by*
`inv_03_cross_document_transplant_is_rejected`.

**INV-4 — transplant between collections is rejected.** Moving a ciphertext into a
collection whose schema differs, under a document id that exists in both, fails
authentication.
*Enforced by* `schema_hash` in the AAD. *Proved by*
`inv_04_cross_collection_transplant_is_rejected`.

**INV-5 — transplant between fields is rejected.** Moving a ciphertext from field
`a` to field `b` in the same document fails authentication.
*Enforced by* `field_path` in the AAD. *Proved by*
`inv_05_cross_field_transplant_is_rejected`.

**INV-6 — rollback within a document is rejected.** Replacing the current
ciphertext for a field with an earlier revision's ciphertext for the same field,
same document, same schema, fails authentication rather than returning stale
plaintext.
*Enforced by* `revision` in the AAD. *Proved by*
`inv_06_revision_rollback_is_rejected`.

**INV-7 — rotation preserves plaintext.** After `rotate()`, every document's every
encrypted field decrypts to exactly its pre-rotation plaintext bytes; every field's
`gen` is the new generation; and no field still carries the old generation.
*Enforced by* rotation re-encrypting from plaintext rather than re-wrapping
ciphertext, and by a completion check driven off the generation byte. *Proved by*
`inv_07_rotation_preserves_plaintext_exactly`.

**INV-8 — a wrong password fails at open, and fails distinctly.** Opening with a
password that differs from the one the database was created with returns
`WrongPassword` before any document is read, and never returns a successful open
followed by a later decryption failure.
*Enforced by* the verifier check against the internal store at open. *Proved by*
`inv_08_wrong_password_fails_at_open`.

**INV-9 — a truncated or extended envelope is rejected.** Flipping any single byte
of the 47-byte header, truncating the body, or appending bytes all fail
authentication or parsing, never return partial plaintext.
*Enforced by* an exact-length parse and the Poly1305 tag covering the body.
*Proved by* `inv_09_every_header_bit_flip_is_rejected` (exhaustive over the header,
not sampled).

**INV-10 — the same plaintext never produces the same ciphertext.** Encrypting the
same value twice for the same document, field, revision and generation yields
different envelopes (fresh nonce), and both decrypt to the same plaintext.
*Enforced by* a fresh random nonce per write. *Proved by*
`inv_10_encryption_is_nondeterministic_but_stable_on_decrypt`.

**INV-11 — no plaintext escapes into an error.** No error value, error message, or
log record produced by this crate contains plaintext, the derived key, the password,
or a KDF salt.
*Enforced by* every error carrying a kind and a static message, with no
`impl Debug for ...` that could interpolate a plaintext buffer. *Proved by*
`inv_11_errors_never_carry_secrets`, which drives the error paths and greps the
formatted output for every plaintext it fed in.

**INV-12 — encrypted paths are not queryable.** A selector or sort that names an
encrypted path is rejected at query preparation with `EncryptedPathInQuery`, and the
underlying storage is never asked.
*Enforced by* a check against the schema's `encrypted` list in the wrapper's query
and count paths. *Proved by* `inv_12_encrypted_paths_are_rejected_in_queries`.

**INV-13 — an unknown version is an error, not a guess.** An envelope whose `version`
or `suite` byte is not `0x01` returns `UnsupportedEnvelope` carrying the byte, for
both the reader and any future reader that gains a new version.
*Enforced by* an exhaustive match on the version and suite bytes with no fallback
arm. *Proved by* `inv_13_unknown_versions_are_rejected_not_guessed`, exhaustive over
all 256x256 version/suite pairs.

**INV-14 — attachments obey the schema flag.** With `attachments.encrypted: true`,
stored attachment bytes do not contain the plaintext; with it `false` or absent, the
bytes are stored verbatim.
*Enforced by* the same envelope applied at the attachment boundary, gated on the
schema flag. *Proved by* `inv_14_attachment_encryption_follows_the_schema_flag`.

**INV-15 — a child of an encrypted parent is a schema error.** `encrypted:
["a", "a.b"]` is rejected at database open, in every build.
*Enforced by* an overlap check over the sorted `encrypted` list at open.
*Proved by* `inv_15_nested_encrypted_paths_are_rejected`.

## 5. Failure modes

| Failure | What the caller sees | What the crate does |
| --- | --- | --- |
| Wrong password | `WrongPassword` at database open | compares a verifier derived from the key against the internal store; never proceeds |
| Password missing on an encrypted database | `PasswordRequired` at open | refuses; no default, no empty-string fallback |
| Password supplied for an unencrypted collection | ignored for that collection | matches upstream: no `encrypted` list means no encryption |
| Unknown envelope version or suite | `UnsupportedEnvelope { version, suite }` | refuses to parse; never guesses |
| Tag mismatch (tampered, transplanted, rolled back) | `AuthenticationFailed` | refuses; the caller learns that the data is not trustworthy, not which check failed in detail (a precise oracle helps an attacker) |
| Truncated envelope | `MalformedEnvelope { reason }` | refuses |
| Ciphertext where the schema expects a non-string type | `MalformedEnvelope` / schema validation, whichever fires first | the wrapper replaces the encrypted field's schema with `{type: "string"}`, as upstream does, so this is a storage-corruption case |
| Rotation interrupted | database remains open and readable | both generations accepted on read until rotation completes; resumable |
| Rotation with a wrong password | `WrongPassword` | the same check, before any rewrite |
| A query names an encrypted path | `EncryptedPathInQuery` | rejected at preparation (INV-12); divergence from upstream, G-031 |
| Attachments encrypted, document fields not | each is handled per its own flag | independent flags, as upstream documents |
| The underlying storage returns a value that is not a dxdb envelope for an encrypted field | `MalformedEnvelope` | never treated as plaintext |
| A schema migration changes the schema hash | every ciphertext fails authentication (INV-4) | **this is a data-loss-shaped event and needs a documented path** — see G-036; the spec's position is that a migration touching an encrypted collection must re-encrypt, and the migration plugin must refuse otherwise |
| Very large field | works; body length is unbounded | memory is the only limit; no chunking in v1 (see section 9) |

Error kinds, complete list: `WrongPassword`, `PasswordRequired`, `UnsupportedEnvelope`,
`AuthenticationFailed`, `MalformedEnvelope`, `EncryptedPathInQuery`,
`PasswordTooWeak`, `EncryptionInWorkerRequired`. That is eight; nothing else is
produced by this crate.

## 6. Performance targets

All **unmeasured**. No bench has been run, and this crate may not be described as
fast, slow, or comparable to anything until section 5 of `docs/BENCHMARKS.md` has a
number.

| Target | Value | Why this target |
| --- | --- | --- |
| KDF cost per database open | **unmeasured**; target 100-400 ms on a mid-range phone, native and wasm | 64 MiB x 3 passes; unlocked once per open (3.7) |
| Field encrypt/decrypt | **unmeasured**; target ≥ 1 GB/s throughput for XChaCha20-Poly1305 on native, ≥ 200 MB/s on wasm | the cipher is not the bottleneck; the KDF is |
| Per-field overhead | 47 raw bytes, 63 base64 characters | by construction, asserted by a test rather than measured |
| Memory during KDF | 64 MiB peak, held by Argon2 | from the RFC parameter `m`; the reason 4.6 cannot be run per document |

Upstream's published claim is "Document inserts are about 10x faster compared to
crypto-js" for its WebCrypto plugin, and "5 times faster" on the performance chart.
Both are targets to beat or lose to on our own hardware, not facts (G-003). We will
not compare against them without measuring both sides, and we cannot measure the
paid plugin at all without a license.

## 7. Non-goals

- **No WebCrypto compatibility.** G-002, and section 0's reasoning. Not now, not
  later, not with a shim.
- **No compatibility with the Apache-2.0 `crypto-js` envelope.** Refused on purpose:
  reproducing `EVP_BytesToKey` and an unauthenticated AES mode would reproduce a
  fast-KDF, no-AAD envelope. G-030.
- **No AES-GCM option in v1.** G-034.
- **No asymmetric encryption.** Upstream's documentation says it is not supported
  either, and offers the standard workaround (encrypt the password with a public
  key). Same non-goal, same reason.
- **No password change in place.** Rotation rotates the *key*, not the password.
  Changing a password is a migration, as upstream documents. G-033.
- **No whole-document encryption.** Only declared fields. Documents must stay
  queryable by primary key, revision and index.
- **No length hiding.** No padding to a fixed block size. It would triple small
  field sizes to hide a length that the storage layer already leaks through other
  channels; if that becomes a requirement it is a single opt-in flag, not the
  default.
- **No compression before encryption.** It leaks plaintext structure through length
  (CRIME-style) and the port has no compression plugin yet.
- **No searchable encryption, deterministic encryption, or order-preserving
  encryption.** All three trade the property that makes encryption worth doing
  against a threat we have, for a query capability that 1.3 deliberately refuses.
- **No key escrow, no recovery path, no backdoor.** A forgotten password means a lost
  database. That is the contract; saying otherwise would be a lie.
- **No `RxDB_UTILS_GLOBAL`/`PREMIUM_FLAG_HASH` machinery, no limits, no phone-home.**
  G-006.
- **No mlock/secure-memory guarantees.** T6 is out of scope (3.1).

## Acceptance list

1. A field in the schema's `encrypted` list is stored as a base64 `DXE1` string and
   is returned as its original JSON value through `find_documents_by_id`. `[INV-1]`
2. A field not in the list is stored verbatim and returned verbatim. `[INV-1]`
3. `encrypted: ["a", "a.b"]` is rejected at database open in a release build.
   `[INV-15]`
4. Opening with a wrong password returns `WrongPassword` before any document read.
   `[INV-8]`
5. Opening an encrypted collection with no password returns `PasswordRequired`.
   `[INV-8]`
6. A single byte flip anywhere in the 47-byte header makes decryption fail.
   `[INV-9]`
7. Truncating the body by one byte makes decryption fail. `[INV-9]`
8. Appending one byte makes decryption fail. `[INV-9]`
9. Moving a ciphertext between two documents under the same schema fails
   authentication. `[INV-3]`
10. Moving a ciphertext between two collections with different schemas fails
    authentication. `[INV-4]`
11. Moving a ciphertext between two fields of one document fails authentication.
    `[INV-5]`
12. Replaying an older revision's ciphertext for the same field fails
    authentication. `[INV-6]`
13. Encrypting the same value twice yields different envelopes that both decrypt to
    the same value. `[INV-10]`
14. After `rotate()`, every encrypted field's plaintext is byte-identical to its
    pre-rotation value, and every field's generation is the new one. `[INV-7]`
15. A rotation interrupted after half the documents is resumable, and during the
    interruption every document is readable. `[INV-7]`
16. A query whose selector names an encrypted path returns `EncryptedPathInQuery`
    and never reaches the inner storage. `[INV-12]`
17. A query whose sort names an encrypted path returns `EncryptedPathInQuery`.
    `[INV-12]`
18. An envelope with any version byte other than `0x01` returns
    `UnsupportedEnvelope` carrying that byte. `[INV-13]`
19. An envelope with any suite byte other than `0x01` returns `UnsupportedEnvelope`.
    `[INV-13]`
20. With `attachments.encrypted: true`, the stored attachment bytes do not contain
    the plaintext; with `false`, they are byte-identical to it. `[INV-14]`
21. No error returned by any path contains the plaintext, the password, the salt, or
    the derived key. `[INV-11]`
22. No value returned by any read operation contains a parseable `DXE1` envelope.
    `[INV-2]`
23. The envelope is exactly 47 bytes of overhead plus the body, and its base64 form
    is exactly 63 characters longer than the plaintext's base64 form for a body
    length that is a multiple of 3. `[INV-9]` (layout is part of the contract)
24. A password below the documented strength floor returns `PasswordTooWeak`; a long
    passphrase returns `Ok`. `[G-032]`

## Test map

Tests live in `crates/cleanroom/dxdb-encryption/tests/`. Names are the contract; the
implementation must supply tests with these names or the spec is not implemented.

| Acceptance | Test | File |
| --- | --- | --- |
| 1, 2 | `acceptance_01_and_02_declared_fields_encrypt_undeclared_fields_do_not` | `tests/acceptance.rs` |
| 3 | `acceptance_03_nested_encrypted_paths_are_a_schema_error` | `tests/acceptance.rs` |
| 4, 5 | `acceptance_04_05_wrong_or_missing_password_fails_at_open` | `tests/acceptance.rs` |
| 6, 7, 8 | `acceptance_06_07_08_tampered_envelopes_are_rejected` | `tests/acceptance.rs` |
| 9 | `acceptance_09_cross_document_transplant_fails` | `tests/acceptance.rs` |
| 10 | `acceptance_10_cross_collection_transplant_fails` | `tests/acceptance.rs` |
| 11 | `acceptance_11_cross_field_transplant_fails` | `tests/acceptance.rs` |
| 12 | `acceptance_12_revision_rollback_fails` | `tests/acceptance.rs` |
| 13 | `acceptance_13_nondeterministic_ciphertext_stable_plaintext` | `tests/acceptance.rs` |
| 14, 15 | `acceptance_14_15_rotation_preserves_plaintext_and_resumes` | `tests/acceptance.rs` |
| 16, 17 | `acceptance_16_17_encrypted_paths_are_rejected_in_queries` | `tests/acceptance.rs` |
| 18, 19 | `acceptance_18_19_unknown_version_and_suite_are_rejected` | `tests/acceptance.rs` |
| 20 | `acceptance_20_attachment_encryption_follows_the_flag` | `tests/acceptance.rs` |
| 21 | `acceptance_21_errors_never_carry_secrets` | `tests/acceptance.rs` |
| 22 | `acceptance_22_no_ciphertext_reaches_the_caller` | `tests/acceptance.rs` |
| 23 | `acceptance_23_envelope_overhead_is_exact` | `tests/acceptance.rs` |
| 24 | `acceptance_24_password_strength_floor` | `tests/acceptance.rs` |
| INV-1 | `inv_01_round_trip_is_byte_identical` | `tests/invariants.rs` |
| INV-2 | `inv_02_no_ciphertext_reaches_the_caller` | `tests/invariants.rs` |
| INV-3 | `inv_03_cross_document_transplant_is_rejected` | `tests/invariants.rs` |
| INV-4 | `inv_04_cross_collection_transplant_is_rejected` | `tests/invariants.rs` |
| INV-5 | `inv_05_cross_field_transplant_is_rejected` | `tests/invariants.rs` |
| INV-6 | `inv_06_revision_rollback_is_rejected` | `tests/invariants.rs` |
| INV-7 | `inv_07_rotation_preserves_plaintext_exactly` | `tests/invariants.rs` |
| INV-8 | `inv_08_wrong_password_fails_at_open` | `tests/invariants.rs` |
| INV-9 | `inv_09_every_header_bit_flip_is_rejected` | `tests/invariants.rs` |
| INV-10 | `inv_10_encryption_is_nondeterministic_but_stable_on_decrypt` | `tests/invariants.rs` |
| INV-11 | `inv_11_errors_never_carry_secrets` | `tests/invariants.rs` |
| INV-12 | `inv_12_encrypted_paths_are_rejected_in_queries` | `tests/invariants.rs` |
| INV-13 | `inv_13_unknown_versions_are_rejected_not_guessed` | `tests/invariants.rs` |
| INV-14 | `inv_14_attachment_encryption_follows_the_schema_flag` | `tests/invariants.rs` |
| INV-15 | `inv_15_nested_encrypted_paths_are_rejected` | `tests/invariants.rs` |

Search space note: INV-9 flips every bit of the 47-byte header (376 cases) and
truncates/appends at every offset within one envelope length (2 x N cases), because
the header is a fixed size and exhaustive is cheap. INV-13 is exhaustive over all
65 536 version/suite pairs. There is no random search anywhere in this spec: every
invariant is either structural or enumerable, which matters more than sample size
for a format that must be reproducible byte for byte.

## Open questions this spec does not settle

Recorded rather than hidden, because an implementer must not invent answers:

1. **The verifier's construction.** Section 3.4 says "a verifier derived from the
   key" is stored in the internal store. Its exact derivation is deliberately
   unspecified here: it must be a *one-way* function of the key that does not reveal
   the key or the password, and the choice interacts with G-036. The implementer
   proposes it in a revision to this section before writing code; the spec is not
   ready to be implemented until this line is replaced.
2. **Attachment chunk size.** INV-14 fixes the behaviour, not the framing. Large
   attachments are encrypted as one body in v1 (section 7); the chunking design, if
   it happens, is a `DXE1` v2 layout change and gets its own spec revision.
3. **The migration path when a schema hash changes.** Section 5 flags it; G-036
   records it as an open engineering problem with a stated interim position (refuse
   rather than corrupt).
