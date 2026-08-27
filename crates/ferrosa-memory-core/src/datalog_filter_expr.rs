//! Parser for Datalog filter expressions.
//!
//! Grammar (high-precedence first):
//! ```text
//!   filter   ::= bool_or
//!   bool_or  ::= bool_and ("||" bool_and)*
//!   bool_and ::= bool_not ("&&" bool_not)*
//!   bool_not ::= "!"* bool_primary
//!   bool_primary ::= "(" filter ")" | str_pred | expr cmp_op expr
//!   str_pred ::= ("str_starts_with"|"str_ends_with"|"str_contains") "(" expr "," expr ")"
//!   cmp_op ::= "==" | "!=" | "<=" | ">=" | "=" | "<" | ">"
//!   expr   ::= term (("+" | "-") term)*
//!   term   ::= factor (("*" | "/" | "%") factor)*
//!   factor ::= number | string_lit | identifier | "(" expr ")" | "-" factor
//! ```
//!
//! See `docs/superpowers/specs/2026-05-02-datalog-filter-grammar-design.md`.

use crate::types::{ArithOp, BuiltinFilter, CmpOp, FilterExpr, StrOp};
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{escaped, is_not, tag},
    character::complete::{char as ch, multispace0, one_of, satisfy},
    combinator::{all_consuming, map, recognize, value},
    multi::{fold_many0, many0, many0_count},
    number::complete::double,
    sequence::{delimited, pair, preceded},
};

fn ws<'a, O, P>(
    mut inner: P,
) -> impl Parser<&'a str, Output = O, Error = nom::error::Error<&'a str>>
where
    P: Parser<&'a str, Output = O, Error = nom::error::Error<&'a str>>,
{
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (i, out) = inner.parse(i)?;
        let (i, _) = multispace0(i)?;
        Ok((i, out))
    }
}

fn number(input: &str) -> IResult<&str, FilterExpr> {
    map(double, |f| {
        FilterExpr::LitNum(ordered_float::OrderedFloat(f))
    })
    .parse(input)
}

fn string_lit(input: &str) -> IResult<&str, FilterExpr> {
    let body = escaped(is_not("\"\\"), '\\', one_of("\"\\nt"));
    let (i, s) = delimited(ch('"'), body, ch('"')).parse(input)?;
    Ok((i, FilterExpr::LitStr(s.to_string())))
}

fn identifier(input: &str) -> IResult<&str, FilterExpr> {
    // Accept both uppercase and lowercase identifiers to support variable names
    // like 'name', 'x', 'X', etc. Datalog filters allow variable names starting
    // with any ASCII letter or underscore.
    let head = satisfy(|c: char| c.is_ascii_alphabetic() || c == '_');
    let tail = many0_count(satisfy(|c: char| c.is_ascii_alphanumeric() || c == '_'));
    let (i, name) = recognize(pair(head, tail)).parse(input)?;
    Ok((i, FilterExpr::Var(name.to_string())))
}

fn parens(input: &str) -> IResult<&str, FilterExpr> {
    delimited(ws(ch('(')), expr, ws(ch(')'))).parse(input)
}

fn neg(input: &str) -> IResult<&str, FilterExpr> {
    let (i, inner) = preceded(ws(ch('-')), factor).parse(input)?;
    Ok((i, FilterExpr::Neg(Box::new(inner))))
}

fn factor(input: &str) -> IResult<&str, FilterExpr> {
    // number is tried before neg so that `-2.5` parses as a single LitNum.
    ws(alt((parens, number, string_lit, identifier, neg))).parse(input)
}

fn term(input: &str) -> IResult<&str, FilterExpr> {
    let (i, init) = factor(input)?;
    fold_many0(
        pair(ws(alt((ch('*'), ch('/'), ch('%')))), factor),
        move || init.clone(),
        |acc, (op, rhs)| FilterExpr::BinOp {
            op: match op {
                '*' => ArithOp::Mul,
                '/' => ArithOp::Div,
                _ => ArithOp::Rem,
            },
            lhs: Box::new(acc),
            rhs: Box::new(rhs),
        },
    )
    .parse(i)
}

fn expr(input: &str) -> IResult<&str, FilterExpr> {
    let (i, init) = term(input)?;
    fold_many0(
        pair(ws(alt((ch('+'), ch('-')))), term),
        move || init.clone(),
        |acc, (op, rhs)| FilterExpr::BinOp {
            op: if op == '+' {
                ArithOp::Add
            } else {
                ArithOp::Sub
            },
            lhs: Box::new(acc),
            rhs: Box::new(rhs),
        },
    )
    .parse(i)
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
    )))
    .parse(input)
}

/// A string-shape predicate: `str_starts_with(S, P)`.
///
/// Tried before the comparison production because its head is a bare
/// identifier, which `expr` would otherwise happily consume as a variable.
/// Negation is not parsed here — `!` belongs to `bool_not` so there is exactly
/// one mechanism for it.
fn str_pred(input: &str) -> IResult<&str, BuiltinFilter> {
    let (i, op) = ws(alt((
        value(StrOp::StartsWith, tag(StrOp::StartsWith.keyword())),
        value(StrOp::EndsWith, tag(StrOp::EndsWith.keyword())),
        value(StrOp::Contains, tag(StrOp::Contains.keyword())),
    )))
    .parse(input)?;
    let (i, (subject, arg)) = delimited(
        ws(ch('(')),
        (expr, preceded(ws(ch(',')), expr)),
        ws(ch(')')),
    )
    .parse(i)?;
    Ok((i, BuiltinFilter::StrPred { op, subject, arg }))
}

/// A comparison, the other leaf of the boolean tree.
fn comparison(input: &str) -> IResult<&str, BuiltinFilter> {
    let (i, (lhs, op, rhs)) = (expr, cmp_op, expr).parse(input)?;
    Ok((i, BuiltinFilter::Compare { op, lhs, rhs }))
}

/// A leaf, or a parenthesised sub-filter.
///
/// `( filter )` is tried first and backtracks, which is what keeps arithmetic
/// working: in `(V + 1) > 3` the parenthesised group is not a filter, so the
/// attempt fails and `comparison` re-reads it as a parenthesised *expression*.
fn bool_primary(input: &str) -> IResult<&str, BuiltinFilter> {
    alt((
        delimited(ws(ch('(')), bool_or, ws(ch(')'))),
        str_pred,
        comparison,
    ))
    .parse(input)
}

fn bool_not(input: &str) -> IResult<&str, BuiltinFilter> {
    let (i, bangs) = ws(many0_count(ch('!'))).parse(input)?;
    let (i, inner) = bool_primary(i)?;
    Ok((
        i,
        if bangs % 2 == 1 {
            BuiltinFilter::Not(Box::new(inner))
        } else {
            inner
        },
    ))
}

fn bool_and(input: &str) -> IResult<&str, BuiltinFilter> {
    let (i, first) = bool_not(input)?;
    let (i, rest) = many0(preceded(ws(tag("&&")), bool_not)).parse(i)?;
    Ok((i, fold_bool(first, rest, BuiltinFilter::All)))
}

fn bool_or(input: &str) -> IResult<&str, BuiltinFilter> {
    let (i, first) = bool_and(input)?;
    let (i, rest) = many0(preceded(ws(tag("||")), bool_and)).parse(i)?;
    Ok((i, fold_bool(first, rest, BuiltinFilter::Any)))
}

/// Keep a lone branch as its bare leaf so a filter that uses no connective
/// serialises exactly as it did before this existed.
fn fold_bool(
    first: BuiltinFilter,
    rest: Vec<BuiltinFilter>,
    wrap: fn(Vec<BuiltinFilter>) -> BuiltinFilter,
) -> BuiltinFilter {
    if rest.is_empty() {
        return first;
    }
    let mut all = Vec::with_capacity(rest.len() + 1);
    all.push(first);
    all.extend(rest);
    wrap(all)
}

fn filter(input: &str) -> IResult<&str, BuiltinFilter> {
    bool_or(input)
}

/// Parse a single filter expression.
///
/// Returns `Err` if the input is not a complete filter (e.g. trailing
/// junk, missing comparison operator, malformed expression). Callers in
/// `datalog.rs` pre-screen for the presence of a comparison operator
/// before dispatching to this function.
pub fn parse_filter(input: &str) -> anyhow::Result<BuiltinFilter> {
    match all_consuming(filter).parse(input) {
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
    #[allow(dead_code)]
    fn var(s: &str) -> FilterExpr {
        FilterExpr::Var(s.to_string())
    }

    #[test]
    fn parses_literal_number_equals_literal() {
        let f = parse_filter("3 == 3").unwrap();
        assert_eq!(
            f,
            BuiltinFilter::Compare {
                op: CmpOp::Eq,
                lhs: lit(3.0),
                rhs: lit(3.0)
            }
        );
    }

    // Task 4: comparison operators + whitespace tolerance
    #[test]
    fn parses_each_comparison_operator() {
        let cases = [
            ("X == Y", CmpOp::Eq),
            ("X = Y", CmpOp::Eq),
            ("X != Y", CmpOp::Ne),
            ("X < Y", CmpOp::Lt),
            ("X <= Y", CmpOp::Le),
            ("X > Y", CmpOp::Gt),
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

    // Task 5: arithmetic precedence, parens, unary minus
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
            BuiltinFilter::Compare {
                lhs:
                    FilterExpr::BinOp {
                        op: ArithOp::Mul,
                        lhs,
                        rhs,
                    },
                ..
            } => {
                assert!(matches!(
                    *lhs,
                    FilterExpr::BinOp {
                        op: ArithOp::Add,
                        ..
                    }
                ));
                assert_eq!(*rhs, lit(2.0));
            }
            other => panic!("expected (X+Y)*2 to parse as Mul at the top, got {other:?}"),
        }
    }

    #[test]
    fn unary_minus_on_variable() {
        let f = parse_filter("-X >= 0").unwrap();
        match f {
            BuiltinFilter::Compare {
                lhs: FilterExpr::Neg(inner),
                ..
            } => {
                assert_eq!(*inner, var("X"));
            }
            other => panic!("expected Neg(Var(X)) on lhs, got {other:?}"),
        }
    }

    #[test]
    fn negative_literal_parses_as_litnum_not_neg() {
        let f = parse_filter("X >= -1.5").unwrap();
        match f {
            BuiltinFilter::Compare {
                rhs: FilterExpr::LitNum(OrderedFloat(v)),
                ..
            } => {
                assert_eq!(v, -1.5);
            }
            other => panic!("expected LitNum(-1.5) on rhs, got {other:?}"),
        }
    }

    // Task 6: string equality + var-to-var comparison
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
            BuiltinFilter::Compare {
                op: CmpOp::Ge,
                lhs: var("S"),
                rhs: var("T")
            }
        );
    }

    // Task 7: error paths
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
}
