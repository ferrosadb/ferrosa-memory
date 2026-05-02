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
    #[allow(dead_code)]
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

}
