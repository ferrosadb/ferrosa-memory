# Datalog Filter Grammar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the Datalog filter grammar in `ferrosa-memory-core` to support all six comparison operators (`==`/`=`/`!=`/`<`/`<=`/`>`/`>=`) and simple arithmetic (`+`/`-`/`*`/`/`, parens, unary minus) on either side, including variable-to-variable comparisons — without breaking already-persisted rules.

**Architecture:** Add a `nom`-based parser module (`datalog_filter_expr.rs`) that produces a new additive `BuiltinFilter::Compare { op, lhs, rhs }` variant. `FilterExpr` is a small AST (`Var | LitNum | LitStr | BinOp | Neg`). The legacy `GreaterThan` / `LessThan` / `NotEqual` variants stay in the enum so persisted CQL rows still deserialize; the parser stops emitting them. The evaluator gains an `eval_expr` helper plus one new arm in `check_one_filter`.

**Tech Stack:** Rust 2024, `nom = "7"`, `nom-supreme = "0.8"`, `ordered-float` (already present), `tracing` (already present). Target crate: `ferrosa-memory-core`.

**Spec:** `docs/superpowers/specs/2026-05-02-datalog-filter-grammar-design.md`

**Branch:** `feat/datalog-filter-grammar`

---

## File Structure

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `crates/ferrosa-memory-core/Cargo.toml` | Add `nom`, `nom-supreme` to `[dependencies]`. |
| Modify | `crates/ferrosa-memory-core/src/types.rs` | Add `CmpOp`, `ArithOp`, `FilterExpr` enums; add `BuiltinFilter::Compare` variant; keep legacy variants. |
| Create | `crates/ferrosa-memory-core/src/datalog_filter_expr.rs` | nom combinators + `pub fn parse_filter`. Contains its own unit tests for parser behaviour. |
| Modify | `crates/ferrosa-memory-core/src/lib.rs` | Wire the new module via `pub mod datalog_filter_expr;`. |
| Modify | `crates/ferrosa-memory-core/src/datalog.rs` | Replace `try_parse_filter` call with the new dispatch; add `eval_expr` + `Compare` arm in `check_one_filter`; update existing parser tests; add new tests. |

Per-file size targets: `datalog_filter_expr.rs` ≤ 350 lines (parser + tests). `types.rs` change is +30 lines. `datalog.rs` net change ≈ +120 lines (eval helper + new tests, minus removed `try_parse_filter`).

---

## Task 1: Add nom dependencies

**Files:**
- Modify: `crates/ferrosa-memory-core/Cargo.toml` (under `[dependencies]`)

- [ ] **Step 1: Add nom and nom-supreme**

Append to the `[dependencies]` block in `crates/ferrosa-memory-core/Cargo.toml`:

```toml
nom = "7"
nom-supreme = "0.8"
```

- [ ] **Step 2: Verify the workspace still type-checks**

Run: `cargo check --package ferrosa-memory-core`
Expected: compiles cleanly, no errors. nom + nom-supreme appear in `Cargo.lock`.

- [ ] **Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/Cargo.toml Cargo.lock
git commit -m "deps: add nom + nom-supreme to ferrosa-memory-core for datalog filter parser"
```

---

## Task 2: Add new enum variants and AST types

**Files:**
- Modify: `crates/ferrosa-memory-core/src/types.rs:362-367`
- Test: `crates/ferrosa-memory-core/src/types.rs` (add tests at the end of the existing `#[cfg(test)] mod tests` block, or create one if absent)

- [ ] **Step 1: Write the failing serde round-trip test**

Append to the test module in `types.rs`:

```rust
#[test]
fn builtin_filter_compare_round_trips_through_json() {
    let f = BuiltinFilter::Compare {
        op: CmpOp::Ge,
        lhs: FilterExpr::Var("S".into()),
        rhs: FilterExpr::BinOp {
            op: ArithOp::Add,
            lhs: Box::new(FilterExpr::Var("T".into())),
            rhs: Box::new(FilterExpr::LitNum(ordered_float::OrderedFloat(1.0))),
        },
    };
    let json = serde_json::to_string(&f).unwrap();
    let back: BuiltinFilter = serde_json::from_str(&json).unwrap();
    assert_eq!(back, f);
}

#[test]
fn legacy_builtin_filter_variants_still_round_trip() {
    let g = BuiltinFilter::GreaterThan("X".into(), 0.5);
    let l = BuiltinFilter::LessThan("X".into(), 0.5);
    let n = BuiltinFilter::NotEqual("X".into(), "Y".into());
    for f in [g, l, n] {
        let json = serde_json::to_string(&f).unwrap();
        let back: BuiltinFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --package ferrosa-memory-core --lib types::tests::builtin_filter_compare_round_trips_through_json types::tests::legacy_builtin_filter_variants_still_round_trip`
Expected: FAIL with "cannot find type `CmpOp`" / "no variant `Compare`" / similar compile errors.

- [ ] **Step 3: Add the new types**

Replace the existing `BuiltinFilter` enum at `crates/ferrosa-memory-core/src/types.rs:362-367` with:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BuiltinFilter {
    /// Legacy: variable greater than a literal float.
    /// No longer emitted by the parser; preserved so already-persisted
    /// `RuleEntry` rows in CQL still deserialize.
    GreaterThan(String, f64),
    /// Legacy. See `GreaterThan` doc.
    LessThan(String, f64),
    /// Legacy. See `GreaterThan` doc.
    NotEqual(String, String),
    /// Full comparison filter — the only variant the parser emits.
    Compare {
        op: CmpOp,
        lhs: FilterExpr,
        rhs: FilterExpr,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CmpOp { Eq, Ne, Lt, Le, Gt, Ge }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilterExpr {
    Var(String),
    LitNum(ordered_float::OrderedFloat<f64>),
    LitStr(String),
    BinOp {
        op: ArithOp,
        lhs: Box<FilterExpr>,
        rhs: Box<FilterExpr>,
    },
    Neg(Box<FilterExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArithOp { Add, Sub, Mul, Div }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --package ferrosa-memory-core --lib types::tests::builtin_filter_compare_round_trips_through_json types::tests::legacy_builtin_filter_variants_still_round_trip`
Expected: PASS.

- [ ] **Step 5: Verify legacy callers still compile**

Run: `cargo check --package ferrosa-memory-core --lib`
Expected: all callers (`datalog.rs`) still compile because legacy variants stayed.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/types.rs
git commit -m "types: add BuiltinFilter::Compare + FilterExpr/CmpOp/ArithOp"
```

---

## Task 3: Scaffold the parser module with a number-only expression test

**Files:**
- Create: `crates/ferrosa-memory-core/src/datalog_filter_expr.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`

- [ ] **Step 1: Wire the module into `lib.rs`**

Find the existing `pub mod datalog;` line in `crates/ferrosa-memory-core/src/lib.rs` and add immediately below:

```rust
pub mod datalog_filter_expr;
```

- [ ] **Step 2: Create the file with the failing test and a stub `parse_filter`**

Create `crates/ferrosa-memory-core/src/datalog_filter_expr.rs`:

```rust
//! Parser for Datalog filter expressions.
//!
//! Grammar (high-precedence first):
//! ```text
//!   filter ::= expr cmp_op expr
//!   cmp_op ::= "==" | "!=" | "<=" | ">=" | "=" | "<" | ">"
//!   expr   ::= term (("+" | "-") term)*
//!   term   ::= factor (("*" | "/") factor)*
//!   factor ::= number | string_lit | identifier | "(" expr ")" | "-" factor
//! ```
//!
//! See `docs/superpowers/specs/2026-05-02-datalog-filter-grammar-design.md`.

use crate::types::{ArithOp, BuiltinFilter, CmpOp, FilterExpr};
use ordered_float::OrderedFloat;

/// Parse a single filter expression.
///
/// Returns `Err` if the input does not contain a top-level comparison
/// operator or if the expression is malformed. Callers in `datalog.rs`
/// pre-screen for the presence of a comparison operator before dispatching
/// to this function.
pub fn parse_filter(input: &str) -> anyhow::Result<BuiltinFilter> {
    let _ = (input, OrderedFloat(0.0_f64), CmpOp::Eq, ArithOp::Add, FilterExpr::Var(String::new()));
    anyhow::bail!("not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_literal_number_equals_literal() {
        let f = parse_filter("3 == 3").unwrap();
        assert_eq!(
            f,
            BuiltinFilter::Compare {
                op: CmpOp::Eq,
                lhs: FilterExpr::LitNum(OrderedFloat(3.0)),
                rhs: FilterExpr::LitNum(OrderedFloat(3.0)),
            }
        );
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --package ferrosa-memory-core --lib datalog_filter_expr::tests::parses_literal_number_equals_literal`
Expected: FAIL with "not yet implemented".

- [ ] **Step 4: Replace the stub with the real implementation**

Replace the entire body of `parse_filter` (and add helper combinators) so the file becomes:

```rust
//! Parser for Datalog filter expressions. See module-level rustdoc above.

use crate::types::{ArithOp, BuiltinFilter, CmpOp, FilterExpr};
use nom::{
    branch::alt,
    bytes::complete::{escaped, is_not, tag},
    character::complete::{char as ch, multispace0, one_of, satisfy},
    combinator::{all_consuming, map, recognize, value},
    multi::{fold_many0, many0_count},
    number::complete::double,
    sequence::{delimited, pair, preceded, tuple},
    IResult,
};

fn ws<'a, O, F>(mut inner: F) -> impl FnMut(&'a str) -> IResult<&'a str, O>
where
    F: FnMut(&'a str) -> IResult<&'a str, O>,
{
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (i, out) = inner(i)?;
        let (i, _) = multispace0(i)?;
        Ok((i, out))
    }
}

fn number(input: &str) -> IResult<&str, FilterExpr> {
    map(double, |f| FilterExpr::LitNum(ordered_float::OrderedFloat(f)))(input)
}

fn string_lit(input: &str) -> IResult<&str, FilterExpr> {
    let body = escaped(is_not("\"\\"), '\\', one_of("\"\\nt"));
    let (i, s) = delimited(ch('"'), body, ch('"'))(input)?;
    Ok((i, FilterExpr::LitStr(s.to_string())))
}

fn identifier(input: &str) -> IResult<&str, FilterExpr> {
    let head = satisfy(|c: char| c.is_ascii_uppercase() || c == '_');
    let tail = many0_count(satisfy(|c: char| c.is_ascii_alphanumeric() || c == '_'));
    let (i, name) = recognize(pair(head, tail))(input)?;
    Ok((i, FilterExpr::Var(name.to_string())))
}

fn parens(input: &str) -> IResult<&str, FilterExpr> {
    delimited(ws(ch('(')), expr, ws(ch(')')))(input)
}

fn neg(input: &str) -> IResult<&str, FilterExpr> {
    let (i, inner) = preceded(ws(ch('-')), factor)(input)?;
    Ok((i, FilterExpr::Neg(Box::new(inner))))
}

fn factor(input: &str) -> IResult<&str, FilterExpr> {
    // number is tried before neg so that `-2.5` parses as a single LitNum.
    ws(alt((parens, number, string_lit, identifier, neg)))(input)
}

fn term(input: &str) -> IResult<&str, FilterExpr> {
    let (i, init) = factor(input)?;
    fold_many0(
        pair(ws(alt((ch('*'), ch('/')))), factor),
        move || init.clone(),
        |acc, (op, rhs)| FilterExpr::BinOp {
            op: if op == '*' { ArithOp::Mul } else { ArithOp::Div },
            lhs: Box::new(acc),
            rhs: Box::new(rhs),
        },
    )(i)
}

fn expr(input: &str) -> IResult<&str, FilterExpr> {
    let (i, init) = term(input)?;
    fold_many0(
        pair(ws(alt((ch('+'), ch('-')))), term),
        move || init.clone(),
        |acc, (op, rhs)| FilterExpr::BinOp {
            op: if op == '+' { ArithOp::Add } else { ArithOp::Sub },
            lhs: Box::new(acc),
            rhs: Box::new(rhs),
        },
    )(i)
}

fn cmp_op(input: &str) -> IResult<&str, CmpOp> {
    // Multi-char operators must come first to avoid prefix collisions.
    ws(alt((
        value(CmpOp::Eq, tag("==")),
        value(CmpOp::Ne, tag("!=")),
        value(CmpOp::Le, tag("<=")),
        value(CmpOp::Ge, tag(">=")),
        value(CmpOp::Eq, tag("=")),
        value(CmpOp::Lt, tag("<")),
        value(CmpOp::Gt, tag(">")),
    )))(input)
}

fn filter(input: &str) -> IResult<&str, BuiltinFilter> {
    let (i, (lhs, op, rhs)) = tuple((expr, cmp_op, expr))(input)?;
    Ok((i, BuiltinFilter::Compare { op, lhs, rhs }))
}

/// Parse a single filter expression.
///
/// Returns `Err` if the input is not a complete filter (e.g. trailing
/// junk, missing comparison operator, malformed expression). Callers in
/// `datalog.rs` pre-screen for the presence of a comparison operator
/// before dispatching to this function.
pub fn parse_filter(input: &str) -> anyhow::Result<BuiltinFilter> {
    match all_consuming(filter)(input) {
        Ok((_, f)) => Ok(f),
        Err(e) => anyhow::bail!("invalid filter '{}': {}", input.trim(), e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ordered_float::OrderedFloat;

    fn lit(n: f64) -> FilterExpr {
        FilterExpr::LitNum(OrderedFloat(n))
    }
    fn var(s: &str) -> FilterExpr {
        FilterExpr::Var(s.to_string())
    }

    #[test]
    fn parses_literal_number_equals_literal() {
        let f = parse_filter("3 == 3").unwrap();
        assert_eq!(
            f,
            BuiltinFilter::Compare { op: CmpOp::Eq, lhs: lit(3.0), rhs: lit(3.0) }
        );
    }
}
```

- [ ] **Step 5: Run the test**

Run: `cargo test --package ferrosa-memory-core --lib datalog_filter_expr::tests::parses_literal_number_equals_literal`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog_filter_expr.rs crates/ferrosa-memory-core/src/lib.rs
git commit -m "feat(datalog): add nom-based filter expression parser scaffold"
```

---

## Task 4: Test all comparison operators

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog_filter_expr.rs` (test module)

- [ ] **Step 1: Add failing tests for each comparison operator**

Append to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn parses_each_comparison_operator() {
    let cases = [
        ("X == Y", CmpOp::Eq),
        ("X = Y",  CmpOp::Eq),
        ("X != Y", CmpOp::Ne),
        ("X < Y",  CmpOp::Lt),
        ("X <= Y", CmpOp::Le),
        ("X > Y",  CmpOp::Gt),
        ("X >= Y", CmpOp::Ge),
    ];
    for (src, want) in cases {
        let got = parse_filter(src).expect(src);
        match got {
            BuiltinFilter::Compare { op, .. } => assert_eq!(op, want, "for {src}"),
            other => panic!("expected Compare for {src}, got {other:?}"),
        }
    }
}

#[test]
fn whitespace_does_not_matter() {
    let a = parse_filter("X>=3").unwrap();
    let b = parse_filter("X >= 3").unwrap();
    let c = parse_filter("X  >=  3").unwrap();
    assert_eq!(a, b);
    assert_eq!(b, c);
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog_filter_expr::tests::parses_each_comparison_operator datalog_filter_expr::tests::whitespace_does_not_matter`
Expected: PASS (the parser already supports all these — this locks the contract).

- [ ] **Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog_filter_expr.rs
git commit -m "test(datalog): cover all comparison operators + whitespace tolerance"
```

---

## Task 5: Test arithmetic and precedence

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog_filter_expr.rs` (test module)

- [ ] **Step 1: Add failing tests for arithmetic precedence and parens**

Append to the test module:

```rust
#[test]
fn arithmetic_precedence_mul_binds_tighter_than_add() {
    let f = parse_filter("X + 2 * Y >= Z").unwrap();
    let want = BuiltinFilter::Compare {
        op: CmpOp::Ge,
        lhs: FilterExpr::BinOp {
            op: ArithOp::Add,
            lhs: Box::new(var("X")),
            rhs: Box::new(FilterExpr::BinOp {
                op: ArithOp::Mul,
                lhs: Box::new(lit(2.0)),
                rhs: Box::new(var("Y")),
            }),
        },
        rhs: var("Z"),
    };
    assert_eq!(f, want);
}

#[test]
fn parens_override_precedence() {
    let f = parse_filter("(X + Y) * 2 < N").unwrap();
    match f {
        BuiltinFilter::Compare { lhs: FilterExpr::BinOp { op: ArithOp::Mul, lhs, rhs }, .. } => {
            assert!(matches!(*lhs, FilterExpr::BinOp { op: ArithOp::Add, .. }));
            assert_eq!(*rhs, lit(2.0));
        }
        other => panic!("expected (X+Y)*2 to parse as Mul at the top, got {other:?}"),
    }
}

#[test]
fn unary_minus_on_variable() {
    let f = parse_filter("-X >= 0").unwrap();
    match f {
        BuiltinFilter::Compare { lhs: FilterExpr::Neg(inner), .. } => {
            assert_eq!(*inner, var("X"));
        }
        other => panic!("expected Neg(Var(X)) on lhs, got {other:?}"),
    }
}

#[test]
fn negative_literal_parses_as_litnum_not_neg() {
    let f = parse_filter("X >= -1.5").unwrap();
    match f {
        BuiltinFilter::Compare { rhs: FilterExpr::LitNum(OrderedFloat(v)), .. } => {
            assert_eq!(v, -1.5);
        }
        other => panic!("expected LitNum(-1.5) on rhs, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog_filter_expr::tests`
Expected: PASS for all six tests in the module.

- [ ] **Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog_filter_expr.rs
git commit -m "test(datalog): cover arithmetic precedence, parens, unary minus"
```

---

## Task 6: Test string literals and var-to-var

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog_filter_expr.rs` (test module)

- [ ] **Step 1: Add tests for string equality and var-to-var ordered comparison**

Append to the test module:

```rust
#[test]
fn string_literal_equality() {
    let f = parse_filter(r#"name == "alice""#).unwrap();
    assert_eq!(
        f,
        BuiltinFilter::Compare {
            op: CmpOp::Eq,
            lhs: var("name"),
            rhs: FilterExpr::LitStr("alice".into()),
        }
    );
}

#[test]
fn variable_to_variable_ordered_comparison() {
    let f = parse_filter("S >= T").unwrap();
    assert_eq!(
        f,
        BuiltinFilter::Compare { op: CmpOp::Ge, lhs: var("S"), rhs: var("T") }
    );
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog_filter_expr::tests::string_literal_equality datalog_filter_expr::tests::variable_to_variable_ordered_comparison`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog_filter_expr.rs
git commit -m "test(datalog): cover string equality + var-to-var comparison"
```

---

## Task 7: Negative-path tests (malformed input)

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog_filter_expr.rs` (test module)

- [ ] **Step 1: Add tests for malformed input**

Append to the test module:

```rust
#[test]
fn rejects_trailing_junk() {
    assert!(parse_filter("X >= 3 garbage").is_err());
}

#[test]
fn rejects_missing_rhs() {
    assert!(parse_filter("X >=").is_err());
}

#[test]
fn rejects_unmatched_paren() {
    assert!(parse_filter("(X + Y >= Z").is_err());
}

#[test]
fn rejects_missing_comparison_operator() {
    // Bare expression with no comparison is not a filter.
    assert!(parse_filter("X + Y").is_err());
}

#[test]
fn error_message_includes_offending_input() {
    let err = parse_filter("X >>= 3").unwrap_err();
    assert!(err.to_string().contains("X >>= 3"));
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog_filter_expr::tests`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog_filter_expr.rs
git commit -m "test(datalog): cover malformed-input error paths"
```

---

## Task 8: Wire `parse_filter` into `parse_rule`

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog.rs` (replace `try_parse_filter` and its call site)

- [ ] **Step 1: Write a failing integration test for `>=` parsing in a full rule**

Locate the existing `#[cfg(test)] mod tests` in `datalog.rs` (around line 690). Append:

```rust
#[test]
fn parse_rule_supports_ge_and_le_via_compare_variant() {
    use crate::types::{CmpOp, FilterExpr};
    use ordered_float::OrderedFloat;

    let rule = parse_rule("hot(X) :- warmth(X, W), W >= 0.5.").unwrap();
    assert_eq!(rule.filters.len(), 1);
    assert_eq!(
        rule.filters[0],
        BuiltinFilter::Compare {
            op: CmpOp::Ge,
            lhs: FilterExpr::Var("W".into()),
            rhs: FilterExpr::LitNum(OrderedFloat(0.5)),
        }
    );

    let rule = parse_rule("cold(X) :- warmth(X, W), W <= 0.1.").unwrap();
    assert_eq!(
        rule.filters[0],
        BuiltinFilter::Compare {
            op: CmpOp::Le,
            lhs: FilterExpr::Var("W".into()),
            rhs: FilterExpr::LitNum(OrderedFloat(0.1)),
        }
    );
}

#[test]
fn parse_rule_supports_arithmetic_filter() {
    use crate::types::{CmpOp, FilterExpr};
    use ordered_float::OrderedFloat;

    let rule = parse_rule("near(X) :- count(X, N), N + 1 < 5.").unwrap();
    let want = BuiltinFilter::Compare {
        op: CmpOp::Lt,
        lhs: FilterExpr::BinOp {
            op: crate::types::ArithOp::Add,
            lhs: Box::new(FilterExpr::Var("N".into())),
            rhs: Box::new(FilterExpr::LitNum(OrderedFloat(1.0))),
        },
        rhs: FilterExpr::LitNum(OrderedFloat(5.0)),
    };
    assert_eq!(rule.filters[0], want);
}
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::parse_rule_supports_ge_and_le_via_compare_variant datalog::tests::parse_rule_supports_arithmetic_filter`
Expected: FAIL — current `try_parse_filter` doesn't recognize `>=` and falls through to `parse_atom` which rejects the body part.

- [ ] **Step 3: Replace the call to `try_parse_filter` with the new dispatch**

In `crates/ferrosa-memory-core/src/datalog.rs`, locate the body-parts loop (around line 70-80) that currently looks like:

```rust
        if let Some(filter) = try_parse_filter(part) {
            filters.push(filter);
        } else {
            body.push(parse_atom(part, &mut anon_counter)?);
        }
```

Replace with:

```rust
        if has_top_level_cmp(part) {
            let f = crate::datalog_filter_expr::parse_filter(part)?;
            filters.push(f);
        } else {
            body.push(parse_atom(part, &mut anon_counter)?);
        }
```

Then add the helper just below `parse_term` (or wherever fits in the file's organization), and **delete** the old `try_parse_filter` function (lines ~170-191):

```rust
/// True iff `s` contains a comparison operator at the top level — i.e.
/// outside string literals and outside parentheses. Used by `parse_rule`
/// to decide whether a body element is a filter or a predicate atom.
fn has_top_level_cmp(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'=' | b'<' | b'>' if depth == 0 => return true,
            b'!' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'=' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}
```

- [ ] **Step 4: Run the new tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::parse_rule_supports_ge_and_le_via_compare_variant datalog::tests::parse_rule_supports_arithmetic_filter`
Expected: PASS.

- [ ] **Step 5: Run the full datalog test module to catch regressions**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests`
Expected: All `parse_rule` tests still pass except the legacy `test_parse_greater_than_filter`, `test_parse_less_than_filter`, and `test_parse_rule_with_filter` which assert the old enum shape — these get fixed in Task 9. Note them but don't fix yet.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): wire parse_filter into parse_rule with top-level cmp pre-scan"
```

---

## Task 9: Update legacy parser tests to expect the `Compare` shape

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog.rs` (tests at lines ~711, ~767, ~774)

- [ ] **Step 1: Update `test_parse_rule_with_filter`**

Replace the existing test at `datalog.rs:711-724`:

```rust
    #[test]
    fn test_parse_rule_with_filter() {
        use crate::types::{CmpOp, FilterExpr};
        let rule =
            parse_rule("related(X, Z) :- co_occurs(X, Y), co_occurs(Y, Z), X != Z.").unwrap();
        assert_eq!(rule.head.predicate, "related");
        assert_eq!(rule.body.len(), 2);
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Ne,
                lhs: FilterExpr::Var("X".into()),
                rhs: FilterExpr::Var("Z".into()),
            }
        );
    }
```

- [ ] **Step 2: Update `test_parse_greater_than_filter`**

Replace the existing test at `datalog.rs:766-771`:

```rust
    #[test]
    fn test_parse_greater_than_filter() {
        use crate::types::{CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;
        let rule = parse_rule("hot(X) :- warmth(X, W), W > 0.5.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Gt,
                lhs: FilterExpr::Var("W".into()),
                rhs: FilterExpr::LitNum(OrderedFloat(0.5)),
            }
        );
    }
```

- [ ] **Step 3: Update `test_parse_less_than_filter`**

Replace the existing test at `datalog.rs:773-778`:

```rust
    #[test]
    fn test_parse_less_than_filter() {
        use crate::types::{CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;
        let rule = parse_rule("cold(X) :- warmth(X, W), W < 0.1.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Lt,
                lhs: FilterExpr::Var("W".into()),
                rhs: FilterExpr::LitNum(OrderedFloat(0.1)),
            }
        );
    }
```

- [ ] **Step 4: Run the datalog test module**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests`
Expected: PASS — all parser tests now align with the new enum shape.

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "test(datalog): update legacy parse-filter tests to assert Compare variant"
```

---

## Task 10: Implement `eval_expr` and the `Compare` evaluator arm

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog.rs` (around the existing `check_one_filter` at line ~351)

- [ ] **Step 1: Write a failing evaluator test**

Append to the `datalog::tests` module:

```rust
#[test]
fn evaluator_handles_ge_and_arithmetic() {
    use crate::types::{CmpOp, FilterExpr, ArithOp};
    use ordered_float::OrderedFloat;
    use std::collections::HashMap;

    let mut binding: HashMap<String, Term> = HashMap::new();
    binding.insert("S".into(), Term::ConstFloat(OrderedFloat(0.7)));
    binding.insert("T".into(), Term::ConstFloat(OrderedFloat(0.5)));

    // S >= T
    let f1 = BuiltinFilter::Compare {
        op: CmpOp::Ge,
        lhs: FilterExpr::Var("S".into()),
        rhs: FilterExpr::Var("T".into()),
    };
    assert!(check_one_filter(&f1, &binding));

    // S < T  (false)
    let f2 = BuiltinFilter::Compare {
        op: CmpOp::Lt,
        lhs: FilterExpr::Var("S".into()),
        rhs: FilterExpr::Var("T".into()),
    };
    assert!(!check_one_filter(&f2, &binding));

    // T + 0.1 == 0.6
    let f3 = BuiltinFilter::Compare {
        op: CmpOp::Eq,
        lhs: FilterExpr::BinOp {
            op: ArithOp::Add,
            lhs: Box::new(FilterExpr::Var("T".into())),
            rhs: Box::new(FilterExpr::LitNum(OrderedFloat(0.1))),
        },
        rhs: FilterExpr::LitNum(OrderedFloat(0.6)),
    };
    assert!(check_one_filter(&f3, &binding));
}

#[test]
fn evaluator_unbound_var_passes_compare_filter() {
    use crate::types::{CmpOp, FilterExpr};
    use std::collections::HashMap;

    let binding: HashMap<String, Term> = HashMap::new();
    let f = BuiltinFilter::Compare {
        op: CmpOp::Gt,
        lhs: FilterExpr::Var("UNBOUND".into()),
        rhs: FilterExpr::LitNum(ordered_float::OrderedFloat(3.0)),
    };
    // Unbound vars pass — same semantics as legacy GreaterThan.
    assert!(check_one_filter(&f, &binding));
}

#[test]
fn legacy_filter_variants_still_evaluate() {
    use std::collections::HashMap;
    use ordered_float::OrderedFloat;

    let mut binding: HashMap<String, Term> = HashMap::new();
    binding.insert("X".into(), Term::ConstFloat(OrderedFloat(0.7)));

    assert!(check_one_filter(&BuiltinFilter::GreaterThan("X".into(), 0.5), &binding));
    assert!(!check_one_filter(&BuiltinFilter::LessThan("X".into(), 0.5), &binding));

    let mut b2: HashMap<String, Term> = HashMap::new();
    b2.insert("A".into(), Term::ConstStr("foo".into()));
    b2.insert("B".into(), Term::ConstStr("bar".into()));
    assert!(check_one_filter(&BuiltinFilter::NotEqual("A".into(), "B".into()), &b2));
}
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluator_handles_ge_and_arithmetic`
Expected: FAIL — `check_one_filter` has no arm for `Compare`. The compile may also error if the match isn't exhaustive; that's the expected red signal.

- [ ] **Step 3: Add `eval_expr` and the `Compare` arm**

In `crates/ferrosa-memory-core/src/datalog.rs`, just above the existing `check_one_filter` function, add:

```rust
/// Runtime value produced by `eval_expr`. Strings, numbers, and UUIDs
/// each have their own arm so type mismatches surface clearly.
enum EvalValue {
    Num(f64),
    Str(String),
    Uuid(uuid::Uuid),
}

fn eval_expr(
    e: &crate::types::FilterExpr,
    binding: &std::collections::HashMap<String, Term>,
) -> Option<EvalValue> {
    use crate::types::{ArithOp, FilterExpr};
    use ordered_float::OrderedFloat;
    match e {
        FilterExpr::Var(name) => match binding.get(name)? {
            Term::ConstFloat(OrderedFloat(f)) => Some(EvalValue::Num(*f)),
            Term::ConstStr(s) => Some(EvalValue::Str(s.clone())),
            Term::Const(u) => Some(EvalValue::Uuid(*u)),
            Term::Var(_) => None,
        },
        FilterExpr::LitNum(OrderedFloat(f)) => Some(EvalValue::Num(*f)),
        FilterExpr::LitStr(s) => Some(EvalValue::Str(s.clone())),
        FilterExpr::Neg(inner) => match eval_expr(inner, binding)? {
            EvalValue::Num(x) => Some(EvalValue::Num(-x)),
            other => {
                tracing::warn!(?other, "datalog: unary minus on non-numeric value");
                None
            }
        },
        FilterExpr::BinOp { op, lhs, rhs } => {
            let l = eval_expr(lhs, binding)?;
            let r = eval_expr(rhs, binding)?;
            match (l, r) {
                (EvalValue::Num(a), EvalValue::Num(b)) => match op {
                    ArithOp::Add => Some(EvalValue::Num(a + b)),
                    ArithOp::Sub => Some(EvalValue::Num(a - b)),
                    ArithOp::Mul => Some(EvalValue::Num(a * b)),
                    ArithOp::Div => {
                        if b == 0.0 {
                            tracing::warn!("datalog: division by zero in filter");
                            None
                        } else {
                            Some(EvalValue::Num(a / b))
                        }
                    }
                },
                _ => {
                    tracing::warn!("datalog: arithmetic on non-numeric values");
                    None
                }
            }
        }
    }
}

fn apply_cmp(op: crate::types::CmpOp, l: &EvalValue, r: &EvalValue) -> bool {
    use crate::types::CmpOp;
    use std::cmp::Ordering;
    match (l, r) {
        (EvalValue::Num(a), EvalValue::Num(b)) => {
            let Some(ord) = a.partial_cmp(b) else { return false; }; // NaN
            match (op, ord) {
                (CmpOp::Eq, Ordering::Equal) => true,
                (CmpOp::Ne, ord) => ord != Ordering::Equal,
                (CmpOp::Lt, Ordering::Less) => true,
                (CmpOp::Le, ord) => ord != Ordering::Greater,
                (CmpOp::Gt, Ordering::Greater) => true,
                (CmpOp::Ge, ord) => ord != Ordering::Less,
                _ => false,
            }
        }
        (EvalValue::Str(a), EvalValue::Str(b)) => {
            let ord = a.cmp(b);
            match (op, ord) {
                (CmpOp::Eq, Ordering::Equal) => true,
                (CmpOp::Ne, ord) => ord != Ordering::Equal,
                (CmpOp::Lt, Ordering::Less) => true,
                (CmpOp::Le, ord) => ord != Ordering::Greater,
                (CmpOp::Gt, Ordering::Greater) => true,
                (CmpOp::Ge, ord) => ord != Ordering::Less,
                _ => false,
            }
        }
        (EvalValue::Uuid(a), EvalValue::Uuid(b)) => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            _ => {
                tracing::warn!(?op, "datalog: ordered comparison on UUID; returning false");
                false
            }
        },
        _ => {
            tracing::warn!(?op, "datalog: type mismatch in filter; returning false");
            false
        }
    }
}
```

Then update `check_one_filter` to handle the new variant. Replace the existing function body (currently three arms) with a four-arm match:

```rust
fn check_one_filter(
    filter: &BuiltinFilter,
    binding: &std::collections::HashMap<String, Term>,
) -> bool {
    match filter {
        BuiltinFilter::NotEqual(lhs, rhs) => {
            let lhs_val = binding.get(lhs);
            let rhs_val = binding.get(rhs);
            match (lhs_val, rhs_val) {
                (Some(l), Some(r)) => l != r,
                _ => true,
            }
        }
        BuiltinFilter::GreaterThan(var, threshold) => {
            if let Some(Term::ConstFloat(ordered_float::OrderedFloat(v))) = binding.get(var) {
                *v > *threshold
            } else {
                true
            }
        }
        BuiltinFilter::LessThan(var, threshold) => {
            if let Some(Term::ConstFloat(ordered_float::OrderedFloat(v))) = binding.get(var) {
                *v < *threshold
            } else {
                true
            }
        }
        BuiltinFilter::Compare { op, lhs, rhs } => {
            let (Some(l), Some(r)) = (eval_expr(lhs, binding), eval_expr(rhs, binding)) else {
                // Unbound or type-mismatch (already warned). Match legacy
                // semantics: partial bindings pass the filter.
                return true;
            };
            apply_cmp(*op, &l, &r)
        }
    }
}
```

- [ ] **Step 4: Run the new evaluator tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluator_handles_ge_and_arithmetic datalog::tests::evaluator_unbound_var_passes_compare_filter datalog::tests::legacy_filter_variants_still_evaluate`
Expected: PASS.

- [ ] **Step 5: Run the full datalog test module**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests`
Expected: PASS — no regressions.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): evaluate BuiltinFilter::Compare with type-mismatch fail-loud"
```

---

## Task 11: Integration smoke test — full rule evaluation with `>=`

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog.rs` (tests module)

- [ ] **Step 1: Add an integration test that derives facts using a `>=` filter**

Append to the `datalog::tests` module:

```rust
#[test]
fn evaluate_full_rule_with_ge_filter() {
    use ordered_float::OrderedFloat;

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();

    let mut facts = FactSet::new();
    // confidence(entity, score)
    facts.insert("confidence", vec![Term::Const(a), Term::ConstFloat(OrderedFloat(0.85))]);
    facts.insert("confidence", vec![Term::Const(b), Term::ConstFloat(OrderedFloat(0.65))]);
    facts.insert("confidence", vec![Term::Const(c), Term::ConstFloat(OrderedFloat(0.7))]);

    let rule = parse_rule("trusted(X) :- confidence(X, S), S >= 0.7.").unwrap();
    let (derived_set, _provenance) = evaluate(&[rule], &facts, 100, 1000);

    let trusted: std::collections::HashSet<Uuid> = derived_set
        .iter("trusted")
        .filter_map(|args| match args.first()? {
            Term::Const(u) => Some(*u),
            _ => None,
        })
        .collect();

    assert!(trusted.contains(&a), "0.85 >= 0.7 should derive trusted(a)");
    assert!(trusted.contains(&c), "0.7 >= 0.7 should derive trusted(c)");
    assert!(!trusted.contains(&b), "0.65 >= 0.7 must not derive trusted(b)");
}
```

> **Note:** if `FactSet::iter` doesn't take a predicate name, adapt the assertion to use whatever public accessor `FactSet` exposes (look at the existing `test_evaluate_triangle` test in the same module for the canonical pattern). The point is to confirm the three entities are filtered correctly.

- [ ] **Step 2: Run the test**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluate_full_rule_with_ge_filter`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "test(datalog): integration test for >= filter end-to-end"
```

---

## Task 11b: Regression tests for user-supplied examples

**Files:**
- Modify: `crates/ferrosa-memory-core/src/datalog.rs` (tests module)

These two examples were called out by the user as the rules they need to write. The first is fully covered by this change; the second needs **aggregation** (`count(predicate(...), N)`), which is out of scope for the arithmetic/comparison patch — it's added as a deliberately `#[ignore]`-d test that documents the next deliverable.

- [ ] **Step 1: Add a passing regression test for the var-to-var inequality example**

Append to the `datalog::tests` module:

```rust
#[test]
fn user_example_var_to_var_inequality() {
    // The user explicitly called this out as a target rule. After this
    // change it parses to a Compare { op: Ne, … } and evaluates correctly.
    use crate::types::{CmpOp, FilterExpr};

    let rule = parse_rule(
        "avoid_action(X) :- user_corrected(S1, X), user_corrected(S2, X), S1 != S2."
    )
    .unwrap();
    assert_eq!(rule.filters.len(), 1);
    assert_eq!(
        rule.filters[0],
        BuiltinFilter::Compare {
            op: CmpOp::Ne,
            lhs: FilterExpr::Var("S1".into()),
            rhs: FilterExpr::Var("S2".into()),
        }
    );

    // End-to-end: two distinct sessions corrected the same target.
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let target = Uuid::new_v4();
    let mut facts = FactSet::new();
    facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target)]);
    facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target)]);
    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
    let any_avoid = derived.iter("avoid_action").next().is_some();
    assert!(any_avoid, "expected avoid_action to fire when two distinct sessions corrected the same target");
}
```

- [ ] **Step 2: Add an `#[ignore]`-d failing test for the aggregation example**

Append to the same test module:

```rust
#[test]
#[ignore = "requires aggregation (count) support — see specs follow-up: \
             count(predicate(...), N) is not in the arithmetic/comparison grammar. \
             Tracking deliverable: extend the parser AST + evaluator with aggregate \
             predicates. Removing #[ignore] without adding aggregation will fail \
             with 'invalid filter' on the count(...) clause."]
fn user_example_count_aggregate_with_ge() {
    // Target rule from the user:
    //   avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.
    //
    // The N >= 3 filter parses fine after this change. The blocker is
    // count(user_corrected(S, X), N) — that's an aggregate predicate, not
    // a plain atom or a filter. parse_rule currently has no notion of
    // aggregation, so this rule fails at parse time on the count(...) body.
    //
    // When the aggregation feature lands, remove #[ignore] and assert the
    // expected derivation:
    //   3 distinct sessions corrected target T  ⇒  avoid_action(T) fires
    //   2 distinct sessions corrected target U  ⇒  avoid_action(U) does NOT fire
    let rule = parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.")
        .expect("aggregation-extended parser should accept this rule");
    let _ = rule;
    panic!("aggregation evaluator not implemented");
}
```

- [ ] **Step 3: Run the regression test**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::user_example_var_to_var_inequality`
Expected: PASS.

- [ ] **Step 4: Verify the ignored test is registered**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::user_example_count_aggregate_with_ge`
Expected: `0 passed; 0 failed; 1 ignored` — test exists in the binary, doesn't run.

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "test(datalog): regression for var-to-var inequality + ignored aggregation TODO"
```

---

## Task 12: Final verification — full crate test, clippy, fmt

**Files:** none modified; verification only.

- [ ] **Step 1: Run the entire ferrosa-memory-core test suite**

Run: `cargo test --package ferrosa-memory-core --lib`
Expected: PASS — all tests across all modules.

- [ ] **Step 2: Run clippy on the whole crate**

Run: `cargo clippy --package ferrosa-memory-core --lib -- -D warnings`
Expected: clean. If any warnings appear in the new code, fix them before continuing. Common ones to watch for:
- `clippy::needless_borrow` in match arms
- `clippy::redundant_clone` in eval_expr
- `clippy::collapsible_match` in apply_cmp

If you fix any clippy warnings, commit with: `git commit -am "style(datalog): fix clippy lints"`.

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check`
Expected: clean. If not, run `cargo fmt` and commit: `git commit -am "style: cargo fmt"`.

- [ ] **Step 4: Verify no unrelated files changed**

Run: `git diff --name-only feat/datalog-filter-grammar feature/ground-truth-p0-gap-1`
Expected: only the files listed in the File Structure section above.

- [ ] **Step 5: Final summary**

Print:
```
Datalog filter grammar implementation complete.
Branch: feat/datalog-filter-grammar
Files changed: <count from previous step>
Tests added: <count> (run `git log --oneline feat/datalog-filter-grammar` for the trail)
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Implementing task |
|---|---|
| Grammar | Tasks 3, 4, 5, 6 |
| `BuiltinFilter::Compare` + `FilterExpr`/`CmpOp`/`ArithOp` types | Task 2 |
| nom dependency add | Task 1 |
| Parser module structure | Tasks 3-7 |
| Multi-char operator precedence (`==` before `=`, etc.) | Task 3 (factor combinator order) + Task 4 (test coverage) |
| Filter-vs-atom dispatch in `parse_rule` | Task 8 (`has_top_level_cmp` helper) |
| `eval_expr` with EvalValue | Task 10 |
| Type-mismatch fail-loud (tracing::warn + return false) | Task 10 (`apply_cmp`) |
| Backward-compat for legacy variants (parse + eval) | Task 2 (legacy serde test) + Task 10 (legacy_filter_variants_still_evaluate) |
| Updated existing parser tests | Task 9 |
| Round-trip serde tests | Task 2 |
| Integration smoke test | Task 11 |
| clippy + fmt gates | Task 12 |

**Placeholder scan:** No "TBD", no "implement later", no "similar to Task N", no "add appropriate error handling". Every task has full code.

**Type consistency:** `BuiltinFilter::Compare { op, lhs, rhs }`, `FilterExpr::Var(String) | LitNum(OrderedFloat<f64>) | LitStr(String) | BinOp { op, lhs, rhs } | Neg(Box<FilterExpr>)`, `CmpOp::Eq | Ne | Lt | Le | Gt | Ge`, `ArithOp::Add | Sub | Mul | Div`. Names used consistently in tests and impl.

**Out-of-scope tagged in spec but deferred:** rewriting `builtin_rules()` to use new operators (no-op refactor), proptest property tests (nice-to-have), CQL migration to drop legacy variants (non-goal).
