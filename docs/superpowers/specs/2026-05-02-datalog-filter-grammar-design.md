# Datalog Filter Grammar — Full Comparisons + Simple Arithmetic

**Status:** Design
**Date:** 2026-05-02
**Component:** `ferrosa-memory-core` / `datalog`
**Related code:**
- `crates/ferrosa-memory-core/src/datalog.rs` (parser, evaluator)
- `crates/ferrosa-memory-core/src/types.rs` (`BuiltinFilter`, `Term`, `DatalogRule`)

## Problem

The current Datalog rule parser at `crates/ferrosa-memory-core/src/datalog.rs:171` (`try_parse_filter`) supports only:

- `X != Y` — variable inequality (string match on bound terms)
- `X > 3.0` — variable greater than a literal float
- `X < 3.0` — variable less than a literal float

Three concrete gaps block authors writing useful rules:

1. **No `>=` / `<=`.** The expression `N >= 3` is mis-parsed: `find('>')` returns offset 2, `s[pos+1..] = "= 3"`, which fails `parse::<f64>()`. The filter falls through to `parse_atom`, which rejects it. Result: parser gives a misleading error, rules can't express closed inequalities.
2. **No equality.** `X == Y` and `X = Y` are unrepresentable; rule authors have to fake equality with double-negation tricks.
3. **No variable-to-variable ordered comparisons or arithmetic.** Right-hand side of `>` / `<` must be a literal `f64`. Rules like `score(E, S), threshold(E, T), S >= T` or `count(E, N), N + 1 < limit` are unwritable.

The user's ask: extend the filter grammar to support full comparison operators and simple arithmetic, so `N >= 3`, `S >= T`, and `X + Y < Z` all parse and evaluate.

## Goals

- Parse all six comparison operators: `==`, `=`, `!=`, `<`, `<=`, `>`, `>=` (treating `=` as a synonym for `==`).
- Parse simple arithmetic on either side of a comparison: `+`, `-`, `*`, `/`, unary `-`, parenthesization. Standard left-associative arithmetic precedence (`* /` binds tighter than `+ -`).
- Allow variable-to-variable, variable-to-literal, and literal-to-literal comparisons.
- Evaluate the new filters against runtime variable bindings without breaking existing semantics for unbound variables.
- Stay backward-compatible with rules already persisted to CQL — old `BuiltinFilter::GreaterThan` / `LessThan` / `NotEqual` rows must still deserialize and evaluate.

## Non-goals

- Recursive function calls, aggregation, or built-ins like `length(X)`. Filter scope stays limited to comparison + arithmetic.
- Logical connectives in filters (`and` / `or` / `not`). Conjunction is already implicit across multiple filter clauses; disjunction is expressed through multiple rules.
- Custom operator overloading or extensible filter registry.
- Migrating existing rules in CQL to the new `Compare` representation.

## Grammar

```
filter      ::= expr cmp_op expr
cmp_op      ::= "==" | "=" | "!=" | "<=" | ">=" | "<" | ">"
expr        ::= term (("+" | "-") term)*
term        ::= factor (("*" | "/") factor)*
factor      ::= number
              | string_lit
              | identifier
              | "(" expr ")"
              | "-" factor
number      ::= float literal (e.g. 3, 3.14, -2.5e10) — parsed via nom::number::complete::double, which consumes any leading sign
string_lit  ::= "\"" escaped_chars "\""
identifier  ::= [A-Z_][A-Za-z0-9_]*       (Datalog variable convention; treated as Var)
```

Precedence, high to low: unary `-`, `* /`, `+ -`, comparison.

Whitespace is permissive between tokens. Operator parsing must list multi-char operators (`==`, `!=`, `<=`, `>=`) before single-char (`=`, `<`, `>`) to avoid prefix collisions.

String literals are valid only as direct operands of `==` / `!=`. If a string flows into an arithmetic node (e.g. `"foo" + 1`), the evaluator returns `false` for the enclosing comparison and emits a `tracing::warn!` (fail-loud per project safety rules).

### Distinguishing filter from atom

In `parse_rule`, each comma-separated body element is currently dispatched to `try_parse_filter` first, then `parse_atom`. The new `parse_filter` follows the same pattern, but with a cheap pre-check: scan the element for a top-level comparison operator (one of `=`, `!=`, `<`, `>`) outside string literals and parens. If found, treat as filter; if not, treat as atom. This avoids ambiguity with predicate atoms like `foo(X, Y)` that contain commas but no comparisons.

## Type changes (`crates/ferrosa-memory-core/src/types.rs`)

`BuiltinFilter` evolves additively. Existing variants stay so persisted rules deserialize cleanly.

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BuiltinFilter {
    // Legacy variants — never emitted by the parser after this change.
    // Kept so RuleEntry rows already in CQL still deserialize.
    GreaterThan(String, f64),
    LessThan(String, f64),
    NotEqual(String, String),
    // New variant — the only one the parser emits going forward.
    Compare { op: CmpOp, lhs: FilterExpr, rhs: FilterExpr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CmpOp { Eq, Ne, Lt, Le, Gt, Ge }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilterExpr {
    Var(String),
    LitNum(OrderedFloat<f64>),
    LitStr(String),
    BinOp { op: ArithOp, lhs: Box<FilterExpr>, rhs: Box<FilterExpr> },
    Neg(Box<FilterExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArithOp { Add, Sub, Mul, Div }
```

`OrderedFloat<f64>` keeps `Eq`/`Hash` consistent with the existing `Term::ConstFloat` representation. No CQL migration is required because the legacy variants stay valid; they simply become parser-write-never, evaluator-still-honored.

## Parser module

New module `crates/ferrosa-memory-core/src/datalog_filter_expr.rs` built on `nom = "7"` with `nom-supreme = "0.8"` for span-aware error reporting. Both crates added to `ferrosa-memory-core/Cargo.toml`.

Public API:

```rust
pub fn parse_filter(input: &str) -> anyhow::Result<BuiltinFilter>
```

Internal combinators (private):

- `number` — `nom::number::complete::double` mapped to `FilterExpr::LitNum`.
- `string_lit` — `delimited(char('"'), escaped_chars, char('"'))` mapped to `FilterExpr::LitStr`.
- `identifier` — `recognize(pair(alpha_or_underscore, alphanumeric_or_underscore_0))` mapped to `FilterExpr::Var`.
- `factor` — `alt((parens(expr), number, string_lit, identifier, neg_factor))`. `number` is tried before `neg_factor`, so `-2.5` parses as `LitNum(-2.5)` (single literal node), while `-X` parses as `Neg(Var("X"))` and `-(X+Y)` as `Neg(BinOp(...))`.
- `term` — `fold_many0(factor, mul_or_div, …)` for left-associative `* /`.
- `expr` — `fold_many0(term, add_or_sub, …)` for left-associative `+ -`.
- `cmp_op` — `alt(("==", "!=", "<=", ">=", "=", "<", ">"))` mapped to `CmpOp`. Multi-char operators listed first.
- `filter` — `(expr, cmp_op, expr)` mapped to `BuiltinFilter::Compare { … }`.

`parse_filter` wraps the top-level combinator with `all_consuming` so trailing junk is rejected. `nom-supreme`'s error converter produces an `anyhow::Error` that includes the offending substring and column number:

```
invalid filter 'X >> 3' at column 3: expected expression after '>>'
```

`try_parse_filter` is removed. The single caller in `parse_rule` (`datalog.rs:76`) is updated to use `parse_filter`.

## Evaluator changes (`crates/ferrosa-memory-core/src/datalog.rs`)

Add a small private value type and an evaluator for `FilterExpr`:

```rust
enum EvalValue {
    Num(f64),
    Str(String),
    Uuid(uuid::Uuid),
}

fn eval_expr(e: &FilterExpr, binding: &HashMap<String, Term>) -> Option<EvalValue> { … }
```

Resolution:

- `FilterExpr::Var(name)` → look up in `binding`.
  - `Term::ConstFloat(OrderedFloat(f))` → `Some(EvalValue::Num(f))`.
  - `Term::ConstStr(s)` → `Some(EvalValue::Str(s.clone()))`.
  - `Term::Const(uuid)` → `Some(EvalValue::Uuid(*uuid))`.
  - `Term::Var(_)` (still unbound) → `None`.
  - Missing key → `None`.
- `FilterExpr::LitNum(OrderedFloat(f))` → `Some(EvalValue::Num(f))`.
- `FilterExpr::LitStr(s)` → `Some(EvalValue::Str(s.clone()))`.
- `FilterExpr::Neg(inner)` → `eval_expr(inner)` must yield `Num(x)`; result is `Some(Num(-x))`. String/UUID under `Neg` returns `None` and emits a `tracing::warn!`.
- `FilterExpr::BinOp { op, lhs, rhs }` → both must evaluate to `Num`; otherwise `None` + warn. Division where rhs is `0.0` returns `None` + warn.

Filter check:

```rust
BuiltinFilter::Compare { op, lhs, rhs } => {
    let (Some(l), Some(r)) = (eval_expr(lhs, binding), eval_expr(rhs, binding))
    else {
        return true; // unbound or type-mismatch already warned — keep existing
                     // "partial bindings pass" semantics for consistency with
                     // legacy NotEqual / GreaterThan / LessThan
    };
    apply_cmp(op, &l, &r)
}
```

`apply_cmp(op, l, r)`:

- `Num` vs `Num`: numeric comparison via `f64::partial_cmp`. NaN on either side → `false` (NaN compares unordered).
- `Str` vs `Str`: lexical comparison via `String::cmp` (already `Ord`).
- `Uuid` vs `Uuid`: only `Eq`/`Ne` are honored. `Lt/Le/Gt/Ge` returns `false` and emits a `tracing::warn!("ordered comparison on UUID")`.
- Cross-type (`Num` vs `Str`, etc.): returns `false` and emits `tracing::warn!("type mismatch in datalog filter")`.

The legacy `BuiltinFilter::GreaterThan`, `LessThan`, `NotEqual` arms in `check_one_filter` stay unchanged — old persisted rules continue to evaluate exactly as they do today.

## Error handling

- Parse-time: `anyhow::Error` with column-aware message. Caller (`parse_rule`) propagates with rule-text context.
- Eval-time: division-by-zero, type mismatches, and ordered comparison on UUIDs return `false` for the affected comparison and emit `tracing::warn!` with the rule body text and the offending variable bindings. Per the project's "fail loud" rules, the warning fires every time the path is taken — no silent suppression.

## Testing

Following the project's TDD + CI gates:

1. **Unit tests in `datalog_filter_expr.rs`**:
   - One green test per comparison operator: `==`, `=`, `!=`, `<`, `<=`, `>`, `>=`.
   - One per arithmetic operator: `+`, `-`, `*`, `/`, unary `-`.
   - Precedence: `X + 2 * Y >= Z` parses with `*` binding tighter than `+`.
   - Parens: `(X + Y) * 2 < N` parses correctly.
   - Whitespace tolerance: `X>=3`, `X >= 3`, `X  >=  3` all parse to the same AST.
   - String equality: `name == "alice"` and `name != "bob"`.
   - Var-to-var: `S >= T`.
   - Lit-to-lit: `3 < 5`.
   - Negative numbers: `X >= -1.5`.
   - Negative results: division by zero; trailing junk; missing RHS; unmatched parens; bare identifier without comparison; comparison nested inside arithmetic (`X + (Y == Z)` rejected).

2. **Round-trip serde test in `types.rs` tests**:
   - Construct each `BuiltinFilter` variant (`GreaterThan`, `LessThan`, `NotEqual`, `Compare` with each `CmpOp` and a representative `FilterExpr` shape).
   - JSON round-trip: serialize → deserialize → assert equality.
   - bincode round-trip: same.

3. **Backward-compat test in `datalog.rs` tests**:
   - Manually construct a `BuiltinFilter::GreaterThan("X".into(), 0.5)`, evaluate against a binding with `X = 0.7`, assert `true`. Repeat for `LessThan`, `NotEqual`. Confirms legacy variants still work end-to-end.

4. **Update existing `parse_rule` tests**:
   - Lines `~770` and `~777` of `datalog.rs` currently assert `BuiltinFilter::GreaterThan("W".into(), 0.5)` and `BuiltinFilter::LessThan("W".into(), 0.1)`. Update to expect the new `Compare` shape.
   - Add fresh tests for `>=`, `<=`, `==`, `=`, var-to-var (`S >= T`), and an arithmetic example (`X + 1 < N`).

5. **Integration smoke** in `evaluate` tests:
   - One end-to-end rule using the new operators: `confidence_high(E) :- entity(E, C), C >= 0.7`. Seed facts, run the fixpoint, assert derivation.

6. **Optional**: a proptest that generates random in-grammar filter expressions, parses → serializes → re-parses → evaluates against a synthetic binding, and asserts pre/post equivalence. Gated under `cfg(test)`; nice-to-have, not required for merge.

## Integration

- **Cargo.toml** (`ferrosa-memory-core`): add `nom = "7"` and `nom-supreme = "0.8"`.
- **`parse_rule`** (`datalog.rs:76`): replace `try_parse_filter(part)` with the new dispatch: pre-scan the body part for a top-level comparison operator; if found, call `parse_filter(part)?` and propagate any parse error (fail-loud — a malformed filter should fail rule load, not silently degrade to an atom parse). If no comparison operator, fall through to `parse_atom`.
- **`builtin_rules`** (`datalog.rs:460`): existing rules keep their current syntax; no rewrite as part of this change. A follow-up sweep can rewrite expressions like `confidence > 0.69999` to `confidence >= 0.7` for clarity.
- **`expert_system.rs`** and other callers: confirmed by grep that no out-of-file callers pattern-match on `BuiltinFilter` variants — they treat the type opaquely.

## Risk & rollout

| Risk | Mitigation |
|------|-----------|
| Old persisted rules fail to deserialize after enum change | Additive variant — old variants kept verbatim. Round-trip serde test exercises both shapes. |
| Parser ambiguity with predicate atoms | Top-level comparison-operator pre-scan in `parse_rule` decides filter-vs-atom before touching the parser. |
| Eval-time type confusion silently passes a filter | All type mismatches emit `tracing::warn!` and return `false` for the comparison. |
| nom dependency bloat | `nom` is widely used in the Rust ecosystem and brings minimal transitive deps. `nom-supreme` is small. Both are MIT/Apache. |
| New code path not exercised | TDD work plan (next phase) writes failing tests first per Kent Beck cycle. |

## Out-of-scope follow-ups

- Sweep `builtin_rules()` to use the cleaner operators where applicable.
- Optional: deprecate the legacy `BuiltinFilter` variants once all CQL-stored rules have been re-emitted by the new parser. A migration tool would scan `RuleEntry` rows, re-parse, rewrite. Not part of this change.
- Optional: extend the grammar with `length(X)`, `contains(X, "foo")`, or other unary built-ins. Would require revisiting the AST.
