# radix_immutable as a Merkle radix tree substrate — engineering audit

*Audit, August 2026. Companion to
[reconciliation-tree-fit.md](./reconciliation-tree-fit.md) (which recommended a bespoke
canonical hash-keyed Merkle radix trie for `ops_root`/`state_root`) and
[zk-provable-dag-snapshots.md](./zk-provable-dag-snapshots.md) (§1.5, §7). Subject:
`/laboratory/radix_immutable` — the user's own immutable patricia/radix trie with Arc
structural sharing (`src/{lib,trie,node,key_converter,prefix_view,util}.rs`, ~2.6 kLoC
incl. tests). Question: is it a sound substrate for content-addressed commitments with
inclusion/non-inclusion proofs over 32-byte blake3 keys, and what exactly must change?*

**Verdict up front: YES, with one structural caveat.** The trie's shape is provably and
empirically a pure function of the final key/value map — canonical under arbitrary
insertion orders and interleaved deletions (property tests added and passing, §1.3). The
Merkle-ization surface is small and localized (§2). The one real design decision is the
**byte-granular (256-ary) fanout**: with a naive flat node encoding, inclusion proofs at
n = 10⁴ are ~9.5 KB (dominated by ~255 root-level sibling hashes), not the 0.5–1 KB the
prior report assumed for binary/4-bit tries. A per-node binary commitment over the child
map (spec-level change only, no data-structure change) recovers ~0.8 KB proofs (§2.4).

---

## 1. Canonicity verdict (Deliverable 1): **PASS**

### 1.1 The invariants, from the code

The canonical compressed-patricia form — *unique* for a given key→value map — is pinned
by four invariants. Both mutation paths preserve all four:

- **I1 — root is permanent and empty-fragment.** Every constructor creates
  `root = TrieNode::new(Vec::new())` (`trie.rs:new_str_key/new_bytes_key/new_with_converter`).
  Insert dispatches below the root without ever giving it a fragment; remove explicitly
  exempts the root from merging (`trie.rs:787` `is_root = Arc::ptr_eq(node, &self.root)`;
  checks at `trie.rs:801` via `maybe_merge(.., is_root)` and `trie.rs:856-858`).
- **I2 — every non-root data-less node has ≥ 2 children** (maximal path compression).
  - Insert (`trie.rs:insert_recursive`, 606–711): the fragment-split path (616–657)
    splits at exactly `common_len = prefix_match(key, fragment)` — a pure function of
    the two byte strings. The split node either carries data (key exhausted at split,
    647–652: data + 1 child) or has exactly 2 children (old-content child at
    `fragment[common_len]`, new leaf at `key[common_len]`). No data-less 1-child node is
    ever created.
  - Remove (`trie.rs:remove_recursive`, 758–882): when a node's data is removed and one
    child remains, `maybe_merge` (767–785) merges `fragment ‖ branch_byte ‖ child_fragment`;
    when a child's deletion leaves a data-less node with one sibling, the inline merge
    (856–870) does the same. Emptied leaves return an empty sentinel
    (`TrieNode::new(Vec::new())`, 799/820) which the parent deletes from its child map
    (842–848).
- **I3 — the branch byte lives in the child-map key, not the child fragment** (insert
  629/640/697, and `get`'s symmetric consumption at 451–456). So each node's fragment is
  exactly the key bytes between two divergence points — determined by the key set alone.
- **I4 — child order is canonical**: `children: BTreeMap<u8, Arc<TrieNode>>`
  (`node.rs:45`) iterates in ascending byte order everywhere (hashing, iteration,
  equality).

Value overwrite (insert of an existing key, `trie.rs:663–673`) replaces `data` on a
path-copied node without touching shape. Removing an absent key returns the original
`Arc` unchanged (807, 834, 881). Given I1–I4, every reachable trie is *the* unique
maximal-compression radix tree of its key set — structure cannot encode history.

**No order-dependent structure was found in code review.** The one fragility worth
flagging (not a canonicity bug today): root-detection inside the remove recursion uses
`Arc::ptr_eq(node, &self.root)` (`trie.rs:787`). No current API can alias the root Arc
into a descendant position, but any future subtree-grafting feature would silently break
the root exemption. When touching this code, pass `is_root` as an explicit recursion
parameter instead.

### 1.2 What "same structure" is checkable from outside

`Trie::eq` (`trie.rs:908–929`) is a genuinely structural assertion: it compares
`root.hash()` (the cached structural hash) **first and returns false on mismatch**, then
confirms with `TrieNode::deep_eq` (`node.rs:220–257`), which compares `key_fragment`,
data, and child maps recursively. So an equality pass asserts *both* identical
structural hashes and byte-identical shape — equivalent to comparing serialized forms.

### 1.3 Empirical verification: **PASS (6/6 new property tests, plus full suite green)**

Added `/laboratory/radix_immutable/tests/canonicity_tests.rs` (left uncommitted, as
directed — it's needed regardless of the Merkle decision). Coverage:

| Test | What it does | Result |
|---|---|---|
| `all_insertion_orders_of_hash_keys_yield_identical_structure` | 6 adversarial 32-byte keys (shared prefixes forcing splits at bytes 1, 15, 29, 30; one disjoint key) — **all 720 insertion orders**, `Trie<[u8;32],u64,BytesKeyConverter<_>>` | PASS |
| `all_insertion_orders_of_string_keys_yield_identical_structure` | 7 nested-prefix string keys incl. the empty key (data at root) — **all 5040 orders** | PASS |
| `random_permutations_of_large_hash_keyset_yield_identical_structure` | ~300 keys: 150 uniform 32-byte + 10 forced-prefix families of 15 (deep splits uniform keys never produce) — 50 seeded shuffles | PASS |
| `interleaved_insert_remove_hash_keys_reach_canonical_structure` | 40 rounds: shuffled insert of final ∪ churn sets, interleaved removes + value-overwrite re-inserts + removes of absent keys → compared against direct sorted **and** reverse-sorted builds of the final set | PASS |
| `interleaved_insert_remove_string_keys_reach_canonical_structure` | 60 rounds of a 400-step random op tape over a 3-letter-alphabet universe (max prefix nesting, internal-node data, empty key), settled to a final set, vs direct build | PASS |
| `remove_all_in_any_order_yields_canonical_empty_trie` | 30 random drain orders → `== Trie::new_bytes_key()` | PASS |

Run: `cargo test` in `/laboratory/radix_immutable` — **all 6 pass; entire suite (76 unit
+ 26 integration + 17 doctests) green**. The pre-existing
`tests/quickcheck_tests.rs::qc_insertion_order_independent` / `qc_canonical_after_remove`
already probed weaker versions of this (forward-vs-reverse order; single remove); the new
tests add exhaustive permutations, 32-byte-hash keys, and long interleaved churn.

Also verified: `cargo build --target wasm32-unknown-unknown` succeeds as-is.

---

## 2. Merkle-ization surface (Deliverable 2)

### 2.1 The existing structural hash, and exactly what must change

`TrieNode::hash` (`node.rs:169–182`) → `calculate_hash` (`node.rs:185–211`) hashes, with
`std::collections::hash_map::DefaultHasher` (SipHash-1-3, 64-bit, keys (0,0)):

1. `key_fragment` (via `Vec<u8>: Hash`, which length-prefixes);
2. a `1u8`/`0u8` data-presence discriminant; if present, **the original full key `K`**
   (`kvp.key.hash`) and the value via `V: Hash`;
3. for each child in BTreeMap (ascending-byte) order: the branch byte, then the child's
   *cached 64-bit hash* (`child.hash().hash(&mut hasher)`) — i.e. it is already a
   bottom-up recursive Merkle *shape*, just with a non-cryptographic, non-canonical,
   64-bit hash.

The recursion structure, caching discipline, and child ordering are exactly right; only
the hash function and byte encoding must be replaced. Concretely:

- **New method, not a retrofit**: add `merkle_hash(&self) -> [u8; 32]` +
  `calculate_merkle_hash()` beside the existing pair, and a second cache field
  `cached_merkle_hash: OnceCell<[u8; 32]>` beside `cached_hash` (`node.rs:53`). Keep the
  legacy `hash()` untouched — it backs `PartialEq`'s fast-reject (`trie.rs:921`) and
  `PrefixView`'s `Hash` impl (`prefix_view.rs:299–309`); the two must never be conflated
  (the legacy one is 64-bit, `DefaultHasher`-based, and not even guaranteed stable
  across Rust releases).
- **Canonical node encoding** (blake3, 256-bit, domain-separated — e.g.
  `blake3::Hasher::new_derive_key("walkie ops_root node v1")` or a leading tag byte):

  ```
  node_hash = blake3(
      u8(frag_len) ‖ fragment              // ≤ 255 for 32-byte keys; fixed-width length
    ‖ u8(has_data) ‖ [ u8(32) ‖ key_bytes  // full key bytes at data nodes (see below)
                     ‖ u32le(val_len) ‖ value_bytes ]
    ‖ u16le(child_count)
    ‖ for each child ascending: u8(branch_byte) ‖ child_hash[32]
  )
  ```

  - **Value bytes**: the current `V: Hash` must be replaced by a canonical encoding —
    add a `ValueToBytes<V>` trait mirroring `KeyToBytes<K>` (`key_converter.rs:8–13`),
    or bound `V: AsRef<[u8]>` for the Merkle methods only. For `ops_root`, `V = ()` (or
    the 32-byte op hash) — trivial. For `state_root`, the resolved-state canonical
    encoding walkie already needs anyway.
  - **Key bytes**: the legacy hash includes the original `K`; for the Merkle spec,
    either drop it (the root→node path plus fragment reconstructs the key exactly, by
    I3) or include the full 32-byte key in data-node encodings. **Recommend including
    it** (jmt does the equivalent): it makes proofs self-contained and removes an
    entire class of path-reconstruction bugs from the Rust↔TS parity surface, at 32
    bytes per leaf.
  - Fixed-width length prefixes (not varints) — one less thing to spec cross-language.
- **Cache/invalidation composes for free.** Nodes are immutable after publication;
  *every* mutation path constructs fresh nodes whose constructors initialize empty
  `OnceCell`s (`node.rs:new` 64–72, `with_key_value` 76–87, `new_value_node` 91–102,
  `with_key_fragment` 140–148, `with_data_option` 151–159, `Clone` 260–275 — the Clone
  impl deliberately drops caches). Untouched subtrees are shared by `Arc` with their
  populated caches intact, so recomputing the root after an insert re-hashes only the
  ~depth new nodes on the copied path. This is precisely the incremental-update
  behavior the snapshot layer needs. Two caveats:
  - `TrieNode`'s fields are `pub` (`node.rs:38–59`) and the type is re-exported
    (`lib.rs:81`); external code could mutate a node in place via `Arc::get_mut` and
    stale the cache. Tighten to `pub(crate)` (or document the invariant) when adding
    the cryptographic hash — a stale *commitment* is much worse than a stale eq-hint.
  - `OnceCell::set` failure is ignored (`node.rs:179`) — benign, since the recomputed
    value is identical (idempotent race).

### 2.2 Where proof extraction hooks in

Both proof kinds fall out of the navigation loop that `get` already implements
(`trie.rs:424–468`); `prove` is that loop plus bookkeeping — no new traversal logic:

- **Inclusion** (`Trie::prove(&self, key: &K) -> Proof`): walk root→node consuming
  `fragment ‖ branch_byte` per step (the loop at `trie.rs:432–459`). Per traversed node
  emit: `key_fragment`, the data digest if present, the descent byte, and the sibling
  `(branch_byte, child_merkle_hash)` pairs (all children except the descended one).
  Verifier recomputes the terminal data-node hash from `(key, value_bytes)`, folds
  upward, compares to the root, and checks the concatenated path bytes equal the key.
- **Non-inclusion**: the same walk, terminated at the divergence point, which is one of
  exactly three cases already distinguished in `get`:
  1. fragment mismatch — `common_len < current.key_fragment.len()` (`trie.rs:437`);
  2. no child for the next byte — `children.get(&next_byte) == None` (`trie.rs:457`);
  3. node reached with key exhausted but no data (`trie.rs:446`/`464–467`).
  The proof is the path to the divergence node plus that node's *full* canonical
  encoding (fragment, data digest, all `(byte, child_hash)` pairs). The verifier checks
  the node hashes into the root at that path position and that the key cannot continue
  below it (Merklix-style exclusion). Because keys are fixed-length, "key is a strict
  prefix of another" never occurs, so case 3 only arises for data-less branch nodes.
- `longest_prefix_match` (`trie.rs:517–563`) is the in-crate template for "walk with
  best-effort terminal state" if a combined prove-or-refute API is preferred.
- **Access problem**: `Trie.root` is `pub(crate)` (`trie.rs:32`) and nothing public
  exposes node Arcs (PrefixView's `subtrie_node` is private too). The proof/hash layer
  must therefore live **inside the crate** (natural: `src/proof.rs` + methods on
  `Trie`/`TrieNode`), not as a wrapper crate.

### 2.3 Proof size at n ≈ 10⁴ uniform 32-byte keys — the fanout caveat

The trie branches on whole bytes (256-ary). For n = 10⁴ uniform keys the expected shape
is: root with ~256 children; level-1 nodes covering ~39 keys with ~36 children; level-2
almost all leaves (fragment ≈ 30 bytes). Typical path: 3 nodes.

- **Flat node encoding**: an inclusion proof carries the traversed nodes' sibling sets —
  ≈ 255 + 35 ≈ **290 sibling hashes ≈ 9.3 KB** (~9.5 KB with fragments/overhead). The
  root level alone contributes ~8 KB for any n ≳ 1500. Non-inclusion is the same or
  smaller (divergence usually at level 1–2). Verification is 3 blake3 calls over ~10 KB
  total — microseconds in wasm. Incremental root update after one insert re-hashes
  ~10 KB — also microseconds.
- **Compressed option (recommended if proofs travel)**: define each node's
  `children_root` as a depth-8 binary Merkle over its 256 child slots (sparse-friendly
  with precomputed empty-subtree hashes — the standard SMT trick), and hash
  `fragment ‖ data ‖ children_root`. The *data structure does not change* — only the
  node-hash spec. Proofs become ~8 sibling hashes per traversed level:
  ≈ (8+8+leaf) ≈ **~20–26 hashes ≈ 0.7–0.9 KB** at n = 10⁴, matching the prior
  report's assumptions. Cost: more spec surface for the TS twin, and root updates
  re-hash the touched per-node mini-trees (still ≪ 1 ms; cache mini-tree internals only
  if profiling ever demands it).

Decide per consumer: for rare snapshot-boot verification, 9.5 KB flat is acceptable and
the spec is maximally simple; for zk-witness or frequent light-client use, take the
per-node binary commitment. Changing the trie's actual radix (nibble/binary
`children` maps) would be a rewrite of `insert_recursive`/`remove_recursive`/`get` — not
worth it when the encoding-level fix achieves the same proof sizes.

### 2.4 Key handling: 32-byte binary keys

Clean. `BytesKeyConverter<K>` requires only `K: AsRef<[u8]> + Clone + Hash + Eq`
(`key_converter.rs:57–61`); `[u8; 32]` satisfies all four, and
`Trie<[u8;32], u64, BytesKeyConverter<[u8;32]>>::new_bytes_key()` is exactly what the
new canonicity tests use — compiles and runs with zero friction. Conversion is
zero-copy (`Cow::Borrowed`). Nothing in the trie is string-oriented; `StrKeyConverter`
is just a sibling converter. Two notes:

- `insert` calls `KC::convert(&key).into_owned()` (`trie.rs:583`) — one 32-byte
  allocation per insert. Irrelevant at walkie scale.
- Mixed-length key sets put data on internal nodes (handled correctly, structurally
  hashed via the has-data discriminant, and covered by the string-key tests). With
  fixed 32-byte keys all data sits at leaves; the Merkle spec can either keep the
  general data-marker encoding (recommended — it is already canonical and costs one
  byte) or additionally enforce fixed key length at the walkie wrapper.

### 2.5 Delete support

Full and canonical: `Trie::remove` (`trie.rs:731–755`) → `remove_recursive` (758–882)
with path compression (`maybe_merge` 767–785, inline merge 856–870) and empty-sentinel
elimination (842–848). Post-delete canonicity is exactly what tests 3, 4, and 6 of §1.3
(plus the pre-existing `qc_canonical_after_remove`) verify empirically. History
truncation is therefore supported for free — the property the MST crate lacked
(upsert-only) in the prior report's comparison. Minor nit: `remove` requires `V: Clone`
and clones the removed value even when the caller discards it.

### 2.6 Depth/node caps (key-grinding guard), wasm/no_std/thread-safety

- **Grinding**: keys are blake3 hashes of op content, so an adversary deepens a path
  only by grinding ops whose hashes share prefixes; a p-byte shared prefix between two
  keys costs ~2^{4p} work (birthday) — ~2⁴⁰ for 10 bytes. Depth is *intrinsically*
  capped at 33 nodes by the 32-byte key length, so this is proof-bloat/DoS-grade, never
  unbounded. Guard placement: a `MAX_PROOF_DEPTH` / per-node-entry check belongs in the
  **walkie wrapper layer** (reject or flag ops whose insertion exceeds depth D, e.g.
  D = 12), not inside the crate — the trie itself needs no structural change, and
  `TrieNode::subtree_size` (`node.rs:109–126`, cached) is already available for
  node-population checks. Write D into the spec from day one (the prior report's risk 1:
  retrofitting caps changes every root — though note a pure depth *reject* rule at the
  op-validity layer, rather than an overflow *restructuring* rule, does not alter the
  hash spec).
- **wasm**: builds for `wasm32-unknown-unknown` today (verified). Sole dependency is
  `once_cell` 1.x; `blake3` (pure-Rust on wasm) is the only addition. Single-threaded
  wasm is the easy case for the `OnceCell` caches.
- **no_std**: not currently (`std::sync::Arc`, `std::collections::BTreeMap`,
  `once_cell::sync`). All are alloc-compatible in principle (`portable-atomic` +
  `once_cell/critical-section` or `race::OnceBox`), but walkie's wasm targets have std —
  don't spend effort here.
- **Thread-safety**: `TrieNode: Send + Sync` when `K, V` are (Arc + sync `OnceCell`);
  immutable-after-publication semantics make concurrent readers trivially safe. The
  `OnceCell::set` race is idempotent (§2.1).

---

## 3. Plan + effort (Deliverable 3)

Ordered changes (all inside `/laboratory/radix_immutable` unless noted):

| # | Change | Where | LoC / effort |
|---|---|---|---|
| 1 | `blake3` dep; `ValueToBytes<V>` trait (mirror of `KeyToBytes`, `key_converter.rs:8`) with `UnitValue`/`HashValue` impls | `Cargo.toml`, `src/key_converter.rs` (or new `src/value_converter.rs`) | ~40 LoC, 0.5 d |
| 2 | `cached_merkle_hash: OnceCell<[u8;32]>` field + `merkle_hash()`/`calculate_merkle_hash()` with the §2.1 canonical encoding (domain-separated, versioned tag) | `src/node.rs` — struct at 36, beside `hash()` at 169; init the field in all six constructors (64, 76, 91, 140, 151, 266) | ~120 LoC, 1 d |
| 3 | `Trie::merkle_root(&self) -> [u8;32]` (empty trie = hash of the empty root node — well-defined since the root always exists, I1); tighten `TrieNode` field visibility to `pub(crate)` or document the no-in-place-mutation invariant | `src/trie.rs` (~32, new method near 117), `src/node.rs` | ~30 LoC, 0.5 d |
| 4 | `src/proof.rs`: `Proof` type (per-level: fragment, descent byte, data digest, sibling `(byte, hash)` list; terminal = Inclusion / FragmentDivergence / MissingChild / NoData), `Trie::prove` cloning `get`'s loop (`trie.rs:424`), standalone `verify(root, key_bytes, expected: Option<&[u8]>, proof)` | new module + `lib.rs` export | ~250–350 LoC incl. tests, 2–3 d |
| 5 | Golden vectors: fixed key/value sets → expected roots and proofs, committed as test data; canonicity tests already in `tests/canonicity_tests.rs` extend to assert `merkle_root` equality (one-line additions) | `tests/` | ~100 LoC, 0.5 d |
| 6 | *(Optional, per §2.3)* per-node binary child-map commitment (`children_root`) with precomputed empty-subtree table | `src/node.rs` encoding only | ~150–200 LoC, +1–2 d |
| 7 | walkie integration: `ops_root` wrapper (insert on op ingest, depth-guard D, snapshot message field), `state_root` flat-Merkle (per prior report, no trie needed) | `walkie-songie/src/room/…` | separate work item |
| 8 | TS twin + shared parity vectors | hhs3-ts | 3–5 d (below) |

Total for the Rust Merkle layer: **~600–850 LoC, roughly 4–7 working days** — consistent
with the prior report's estimate for the bespoke option, except the trie itself is
already written, tested, and canonical.

**Rust↔TS byte-identical spec: feasible, and unusually small.** The hash is a pure
function of `(fragment, value_bytes, sorted (byte, child_hash) pairs)` — the TS twin
needs *none* of the Arc/OnceCell/path-copying machinery, just any map that can reproduce
the same compressed-trie shape (or even a batch build from a sorted key list) plus the
§2.1 encoding. The spec hazards to pin in writing: fixed-width length fields; whether
leaf encodings include the full key (recommend yes); the domain-separation string and a
format version byte; ascending-byte child order; the empty-trie root; and (if taken)
the per-node child-commitment layout. BTreeMap ascending-`u8` order and byte-granular
`prefix_match` (`util.rs:4–10`) have exact, trivial TS equivalents.

### Risks

1. **Fanout/proof-size decision (the biggest).** The crate's 256-ary branching makes
   flat proofs ~9.5 KB at 10⁴ keys — fine for snapshot-boot, wrong if proofs need to be
   compact (light clients, zk witnesses). The per-node binary child commitment (§2.3)
   fixes it at spec level, but it must be chosen *before* the first frozen root — it
   changes every hash. Decide now, once.
2. **Two hashes, one crate.** The legacy 64-bit `DefaultHasher` structural hash stays
   (it backs `PartialEq`/`PrefixView::Hash`); nothing must ever surface it as a
   commitment. Naming (`merkle_hash` vs `hash`) and doc comments should make misuse
   loud; consider deprecating public exposure of the u64 hash.
3. **`pub` node fields + `Arc::get_mut`** could mutate a published node and stale a
   cached cryptographic hash — a silent commitment corruption. Tighten visibility in
   step 3; cheap now, painful later.
4. **Root-detection by pointer** (`trie.rs:787`) is correct today but couples remove's
   canonicity to "no API ever aliases the root Arc downward." Convert to an explicit
   `is_root` recursion parameter when editing remove.
5. **Spec drift Rust↔TS.** Mitigated the standard way: golden vectors generated by the
   Rust side, replayed byte-for-byte in TS CI (the prior report's plan). Include
   *proof* vectors, not just roots — verification logic drifts too.

---

## Bottom line

The crate already is the "bespoke canonical hash-keyed radix trie" the prior report
recommended building — canonical by construction (now property-tested exhaustively,
including deletions and 32-byte keys), structurally shared, wasm-clean, delete-capable,
with the recursive hash skeleton and cache discipline already in place. Merkle-izing it
is a contained ~1-week addition (new 256-bit hash + canonical encoding + proof walk),
not a rewrite. The single decision that must be made before freezing anything is the
node-encoding treatment of the 256-way child map (flat ≈ 9.5 KB proofs vs per-node
binary commitment ≈ 0.8 KB), because it is baked into every root forever.
