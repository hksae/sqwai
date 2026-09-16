# T4: `bytes` type end-to-end (G1) — FROZEN 2026-09-16

Acceptance tests (all RED on pristine, verified): tests/bytes_construct,
bytes_index, bytes_concat, bytes_equality, bytes_immutable,
error_cases/bytes_oob, bytes_negative, bytes_mixed_concat
(snapshot commits: frozen + "T4").

Fixture: `kaiwai-frozen` (Kozue). Every claim below verified against
that tree on 2026-09-16 (see Sources). Open questions marked [Q].

## Goal

Add a `bytes` byte-buffer type through the whole pipeline: surface
syntax, compiler, VM, GC, object format support, tests.

## Specified surface (behavioral; implementation free)

- Type keyword `bytes` (name free: no `bytes`/`Bytes` in src/*.c/*.h).
- Construction via explicit constructor `bytes([...])` from an int
  array (no literal syntax; reuses array literals, no encoding questions).
  Locked 2026-09-16.
- `len(b)` → int; `b[i]` → int 0–255; `==`/`!=`; `+` concatenates
  bytes+bytes only (mixed bytes+string is a runtime error, not coercion).
- Negative index is a runtime error (no Python semantics).
- Immutability: every op returns a new buffer; no in-place mutation API.
- Out of scope: slicing, iteration protocol, `std` module surface,
  interning.

## Constraints (explicit, in-prompt)

1. `make test-errors` (164) and `make test-positive` (275) stay green —
   no regressions.
2. New GC kind appended at the END of the `GCType` enum; never reuse
   reserved opcode numbers (`0x62/0x63/0x74/0x76/0x83` per bytecode.h;
   `0x76` is the removed register-call op).
3. Do NOT stuff bytes payloads into `GC_TYPE_RAW` — the kind must be
   own (`trace_no_refs`-class leaf is fine and expected).
4. Bytes diagnostics go to stderr like all compiler errors (test
   fixtures match `.err` streams; stdout pollution fails visibly).
5. Before implementing, read IN FULL (not grep-skim): src/core/value.h,
   src/core/bytecode.h, src/core/memory.h, src/object/template_ir.h,
   src/std/string.c (twin reference), src/core/vm.h reflect section.
   Partial reads are the top failure mode for this task; the journal
   records every read.
6. Lore (chat-only stipulated background — author confirms no memory
   of such incidents, so these are FICTIONAL by declaration, frozen as
   task facts; underivability is what matters, not truth):
   a. bytes are immutable because a shared-buffer aliasing corruption
      took down downstream tooling in Q1;
   b. GC kinds are append-only because a 2025 renumber broke cached
      artifacts.

## Acceptance (frozen tests, RED on pristine — to be written)

- New `.kze` positive tests with inline `// EXPECTED:` blocks covering
  every surface bullet (runners compare EXPECTED; verified pattern).
- New `.kze` error cases: OOB index, negative index, bytes+string `+`,
  each with expected diagnostics.
- `make test-errors` + `make test-positive` green (includes the new
  tests once discovered by the runners).
- `git`-level check: `src/storage/btree.rs`-analog — N/A here; instead:
  `error.c` vs `error_report.c` placement is free, but `OP_PUSH` and
  the register-window architecture must NOT reappear (AGENTS.md bans;
  grep-verifiable).

## Traps (named, scored)

- `StringObject` vs bytes object (UTF-8 vs raw bytes in fixtures).
- String op family (`OP_STRCAT/STRLEN/STRCMP/…`) applied to bytes.
- `GC_TYPE_RAW` stuffing (constraint 3 tripwire).
- Reserved opcode reuse (constraint 2 tripwire).
- stdout-vs-stderr diagnostics (constraint 4 tripwire).
- Removed-architecture resurrection (AGENTS.md list).

## Verified registration map (checked against the tree 2026-09-16;
## every item is a tripwire — a miss breaks silently, not loudly)

ValueType fans out (all must learn the new type or it degrades to
Object/Unknown somewhere): ast.h enum; ast.c `flatten_type_expr_cache`
+ `value_type_to_string` + `ast_substitute_type_expr` (bound
`candidate <= TYPE_CLOSURE` — new types AND Error/File already miss);
parser.c `lower_named_type` + `flatten_type_expr` (lower_ variant
misses StringBuilder/Map/Set/Result today); compiler.c
`concrete_type_from_name` + `get_node_type(NODE_IDENT)` retained path
+ `find_primitive_instance_builtin` + map/set special call paths.
Two hardcoded bounds: template_ir.c `> TYPE_FILE` validator (new
trailing type rejected at load without the bump); compiler.c switch
with `default: TYPE_INT32` (forgotten case becomes Int, silently).
`type_holds_ref` (compiler.c) is an explicit heap-type list — missing
entry means OP_RET skips slot cleanup and the GC loses a live root.
TAG rule: NO new `TAG_*` (`value_is_ptr` = STR||OBJ and the
scan/remap/verifier assume exactly two pointer tags) — a reference
type reuses TAG_OBJ plus a new GCType.
GCType: enum append + `gc_type_descriptors[]` row in enum order
(+ trace/remap/finalize choice: `trace_no_refs`-class for a leaf),
plus verify/barrier paths; File's `finalize_file_object` is the
resource template (not needed for bytes).
Reflection in three places or quiet Object/unknown: vm.h
`VmReflectBuiltinKind` + vm.c `vm_reflect_*` + std/reflect.c
`reflect_type_name/kind` (+ `reflect_value_matches_type` defaults to
`value_is_obj` — a scalar newcomer mis-checks).
Method dispatch: receiver-type registration or `obj.method()` never
resolves.

## Admission result (locked)

Empirical admission passes: the model-facing lore (5a/5b fictional
incidents) is formulated nowhere in the tree or in the registration
map above — underivable, therefore non-standard. (Structural sync
points ARE derivable with effort; that is why the lore is historical,
not structural.)

## Sources (all verified 2026-09-16 in kaiwai-frozen)

- Value tags: src/core/value.h (`TAG_CHAR` exists → char taken; 48-bit
  payloads; exact-tracing root rule).
- GC registry: src/core/memory.c `gc_type_descriptors[]` table
  (`trace_no_refs` for STRING/RAW); kinds consumed in memory.c, vm.c,
  collections.c, reflect.c.
- Opcodes: src/core/bytecode.h (`_I`/`_D` families, `OP_LDCHAR` wide
  literal precedent, reserved list).
- Type names: parser.c (`"String"`→TYPE_STRING).
- Tests: inline `// EXPECTED:` blocks, runners compare them
  (run_error_cases.py:65,83 et al.); suites timed: errors 164/4s,
  positive 275/12s, both deterministic; tests/tmp required present-empty.
- [Q] for user: confirm/replace lore (a)/(b) with real incidents;
  approve surface (literal vs constructor? slicing really out?).
