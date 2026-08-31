//! GeoJSON (RFC 7946) as something a rule can reason over.
//!
//! Two halves, split the way the rest of this engine splits:
//!
//! - The **type vocabulary** — Point, Polygon, Feature and the rest — is
//!   ontology, and lives in [`crate::ontology`] beside every other class.
//! - The **spatial questions** are COMPUTED. Whether a point is inside a
//!   polygon is a calculation over coordinates, not a fact anyone should have
//!   to assert and then keep true.
//!
//! `geo(S)` is the bridge, and it is the same bridge `date(S)` is: geometry
//! arrives as JSON text, exactly as timestamps arrive as strings, and without
//! a way to read it a geometry value would have nothing to compare against.
//!
//! ## What is deliberately refused
//!
//! Only what can be computed *exactly* is answered. Point-in-polygon and
//! point-to-point distance are exact. Polygon-to-polygon overlap is not
//! attempted: a bounding-box approximation would return an answer nobody
//! could distinguish from a real one, which is the failure this codebase
//! keeps refusing. Those return Undefined and say why.

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    const SF: &str = r#"{"type":"Point","coordinates":[-122.4194,37.7749]}"#;
    const NYC: &str = r#"{"type":"Point","coordinates":[-74.0060,40.7128]}"#;
    // A square around San Francisco.
    const BAY: &str = r#"{"type":"Polygon","coordinates":[[
        [-123.0,37.0],[-122.0,37.0],[-122.0,38.0],[-123.0,38.0],[-123.0,37.0]]]}"#;
    // The same square with a hole cut out of the middle, over SF itself.
    const BAY_HOLE: &str = r#"{"type":"Polygon","coordinates":[
        [[-123.0,37.0],[-122.0,37.0],[-122.0,38.0],[-123.0,38.0],[-123.0,37.0]],
        [[-122.5,37.7],[-122.3,37.7],[-122.3,37.9],[-122.5,37.9],[-122.5,37.7]]]}"#;

    // ── Parsing ───────────────────────────────────────────────────

    #[test]
    fn a_point_parses_longitude_first() {
        // RFC 7946 is [longitude, latitude], which is the reverse of how
        // people say it. Getting this backwards puts San Francisco in China
        // and nothing about the result looks wrong.
        let g = Geometry::parse(SF).expect("parses");
        match g {
            Geometry::Point { lon, lat } => {
                assert!((lon - -122.4194).abs() < 1e-9, "longitude first");
                assert!((lat - 37.7749).abs() < 1e-9);
            }
            other => panic!("expected a Point, got {other:?}"),
        }
    }

    #[test]
    fn every_geojson_geometry_type_parses() {
        for (name, json) in [
            ("Point", SF),
            ("LineString", r#"{"type":"LineString","coordinates":[[0,0],[1,1]]}"#),
            ("Polygon", BAY),
            ("MultiPoint", r#"{"type":"MultiPoint","coordinates":[[0,0],[1,1]]}"#),
            ("MultiLineString", r#"{"type":"MultiLineString","coordinates":[[[0,0],[1,1]]]}"#),
            ("MultiPolygon", r#"{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]}"#),
            ("GeometryCollection", r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[0,0]}]}"#),
        ] {
            assert!(Geometry::parse(json).is_some(), "{name} did not parse");
        }
    }

    #[test]
    fn a_feature_yields_its_geometry() {
        let f = format!(r#"{{"type":"Feature","geometry":{SF},"properties":{{"name":"office"}}}}"#);
        assert!(matches!(Geometry::parse(&f), Some(Geometry::Point { .. })));
    }

    #[test]
    fn something_that_is_not_geojson_is_refused_rather_than_guessed() {
        for bad in [
            "not json at all",
            r#"{"type":"Point"}"#,                      // no coordinates
            r#"{"type":"Point","coordinates":[1]}"#,    // not a position
            r#"{"type":"Wormhole","coordinates":[0,0]}"#,
            r#"{"coordinates":[0,0]}"#,                 // no type
        ] {
            assert!(Geometry::parse(bad).is_none(), "should refuse: {bad}");
        }
    }

    // ── The questions that can be answered exactly ────────────────

    #[test]
    fn a_point_inside_a_polygon_is_within_it() {
        let sf = Geometry::parse(SF).unwrap();
        let bay = Geometry::parse(BAY).unwrap();
        assert_eq!(sf.within(&bay), Some(true));
    }

    #[test]
    fn a_point_outside_a_polygon_is_not_within_it() {
        let nyc = Geometry::parse(NYC).unwrap();
        let bay = Geometry::parse(BAY).unwrap();
        assert_eq!(nyc.within(&bay), Some(false));
    }

    #[test]
    fn a_hole_is_not_inside_the_polygon_that_has_it() {
        // The commonest way a point-in-polygon test is wrong.
        let sf = Geometry::parse(SF).unwrap();
        let holed = Geometry::parse(BAY_HOLE).unwrap();
        assert_eq!(sf.within(&holed), Some(false), "SF sits in the hole");
    }

    #[test]
    fn a_point_on_the_boundary_counts_as_inside() {
        // An arbitrary choice, so it is made once, here, and tested — rather
        // than falling out of whichever inequality got typed.
        let edge = Geometry::parse(r#"{"type":"Point","coordinates":[-123.0,37.5]}"#).unwrap();
        let bay = Geometry::parse(BAY).unwrap();
        assert_eq!(edge.within(&bay), Some(true));
    }

    #[test]
    fn a_multipolygon_is_within_if_any_of_its_parts_contains_the_point() {
        let mp = Geometry::parse(
            r#"{"type":"MultiPolygon","coordinates":[
                [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0],[0.0,0.0]]],
                [[[-123.0,37.0],[-122.0,37.0],[-122.0,38.0],[-123.0,38.0],[-123.0,37.0]]]]}"#,
        )
        .unwrap();
        assert_eq!(Geometry::parse(SF).unwrap().within(&mp), Some(true));
    }

    #[test]
    fn distance_between_two_points_is_the_great_circle_distance() {
        let sf = Geometry::parse(SF).unwrap();
        let nyc = Geometry::parse(NYC).unwrap();
        let m = sf.distance_to(&nyc).expect("both are points");
        // ~4,130 km. Loose because the earth is not a sphere and this is.
        assert!(
            (4_120_000.0..4_140_000.0).contains(&m),
            "expected about 4130km, got {}km",
            m / 1000.0
        );
    }

    #[test]
    fn the_distance_from_a_point_to_itself_is_zero() {
        let sf = Geometry::parse(SF).unwrap();
        assert_eq!(sf.distance_to(&sf), Some(0.0));
    }

    // ── The questions that are refused ────────────────────────────

    #[test]
    fn polygon_overlap_is_refused_rather_than_approximated() {
        // A bounding-box answer would be indistinguishable from a real one,
        // and wrong for any concave or diagonal shape.
        let a = Geometry::parse(BAY).unwrap();
        let b = Geometry::parse(BAY_HOLE).unwrap();
        assert_eq!(a.within(&b), None, "polygon-in-polygon is not attempted");
    }

    #[test]
    fn distance_involving_a_polygon_is_refused_rather_than_using_a_centroid() {
        let sf = Geometry::parse(SF).unwrap();
        let bay = Geometry::parse(BAY).unwrap();
        assert_eq!(bay.distance_to(&sf), None);
        assert_eq!(sf.distance_to(&bay), None);
    }

    // ── Through the rule language ─────────────────────────────────

    #[test]
    fn a_rule_can_ask_what_is_inside_a_region() {
        use crate::types::{FactSet, Term};
        let s = |x: &str| Term::ConstStr(x.to_string());
        let mut f = FactSet::new();
        f.insert("site", vec![s("office"), s(SF)]);
        f.insert("site", vec![s("branch"), s(NYC)]);
        f.insert("region", vec![s("bay"), s(BAY)]);

        let rule = crate::datalog::parse_rule(
            r#"in_bay(X) :- site(X, G), region("bay", R), geo_within(geo(G), geo(R))."#,
        )
        .expect("parses");
        let (all, _) = crate::datalog::evaluate(&[rule], &f, 100, 10_000);
        assert!(all.contains("in_bay", &[s("office")]));
        assert!(!all.contains("in_bay", &[s("branch")]));
    }

    #[test]
    fn a_rule_can_ask_what_is_near_something() {
        use crate::types::{FactSet, Term};
        let s = |x: &str| Term::ConstStr(x.to_string());
        let mut f = FactSet::new();
        f.insert("site", vec![s("office"), s(SF)]);
        f.insert("site", vec![s("branch"), s(NYC)]);
        f.insert("hq", vec![s(SF)]);

        let rule = crate::datalog::parse_rule(
            "near_hq(X) :- site(X, G), hq(H), geo_distance(geo(G), geo(H)) < 100000.",
        )
        .expect("parses");
        let (all, _) = crate::datalog::evaluate(&[rule], &f, 100, 10_000);
        assert!(all.contains("near_hq", &[s("office")]), "0km away");
        assert!(!all.contains("near_hq", &[s("branch")]), "4130km away");
    }

    #[test]
    fn a_malformed_geometry_stops_the_rule_rather_than_passing_it() {
        use crate::types::{FactSet, Term};
        let s = |x: &str| Term::ConstStr(x.to_string());
        let mut f = FactSet::new();
        f.insert("site", vec![s("broken"), s("not geojson")]);
        f.insert("region", vec![s("bay"), s(BAY)]);
        let rule = crate::datalog::parse_rule(
            r#"in_bay(X) :- site(X, G), region("bay", R), geo_within(geo(G), geo(R)) || 1 == 1."#,
        )
        .unwrap();
        let (all, _) = crate::datalog::evaluate(&[rule], &f, 100, 10_000);
        assert!(
            all.get("in_bay").map(|r| r.is_empty()).unwrap_or(true),
            "bad geometry is an error, and an error must not be masked by a true sibling"
        );
    }
}
