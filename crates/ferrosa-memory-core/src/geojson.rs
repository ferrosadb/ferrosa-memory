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

/// A geometry, ready to be asked about.
///
/// Wraps `geo_types` rather than reimplementing it. Point-in-polygon with
/// holes, DE-9IM and geodesic distance are each a well-understood problem with
/// a well-tested answer, and a hand-rolled version would be a worse one that
/// looked the same.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry(pub geo::Geometry<f64>);

/// The DE-9IM relations, named as a rule would ask them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpatialRelation {
    Within,
    Contains,
    Intersects,
    Disjoint,
    Touches,
    Crosses,
    Overlaps,
    Equals,
}

impl SpatialRelation {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Within => "geo_within",
            Self::Contains => "geo_contains",
            Self::Intersects => "geo_intersects",
            Self::Disjoint => "geo_disjoint",
            Self::Touches => "geo_touches",
            Self::Crosses => "geo_crosses",
            Self::Overlaps => "geo_overlaps",
            Self::Equals => "geo_equals",
        }
    }

    pub const ALL: [SpatialRelation; 8] = [
        SpatialRelation::Within,
        SpatialRelation::Contains,
        SpatialRelation::Intersects,
        SpatialRelation::Disjoint,
        SpatialRelation::Touches,
        SpatialRelation::Crosses,
        SpatialRelation::Overlaps,
        SpatialRelation::Equals,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.keyword() == name)
    }
}

impl Geometry {
    /// Read any RFC 7946 document that carries a geometry.
    ///
    /// A `Feature` yields its geometry and a `FeatureCollection` yields all of
    /// them, because a rule asking "is this inside that" does not care which
    /// wrapper the geometry arrived in.
    ///
    /// `None` for anything that is not GeoJSON. Guessing would put a rule's
    /// answer on a shape nobody supplied.
    pub fn parse(text: &str) -> Option<Self> {
        use std::convert::TryFrom;
        let parsed: geojson::GeoJson = text.parse().ok()?;
        let geom = match parsed {
            geojson::GeoJson::Geometry(g) => geo::Geometry::try_from(g).ok()?,
            geojson::GeoJson::Feature(f) => geo::Geometry::try_from(f.geometry?).ok()?,
            geojson::GeoJson::FeatureCollection(fc) => {
                let parts: Vec<geo::Geometry<f64>> = fc
                    .features
                    .into_iter()
                    .filter_map(|f| f.geometry)
                    .filter_map(|g| geo::Geometry::try_from(g).ok())
                    .collect();
                if parts.is_empty() {
                    return None;
                }
                geo::Geometry::GeometryCollection(geo::GeometryCollection(parts))
            }
        };
        Some(Geometry(geom))
    }

    /// Ask a DE-9IM relation.
    ///
    /// `None` where the underlying relate engine cannot answer for the pair of
    /// shapes given — refused rather than defaulted to false, because "no" and
    /// "cannot say" are different answers and a rule should not fire on the
    /// second thinking it got the first.
    pub fn relates(&self, relation: SpatialRelation, other: &Geometry) -> Option<bool> {
        use geo::relate::Relate;
        let im = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.relate(&other.0)))
            .ok()?;
        Some(match relation {
            SpatialRelation::Within => im.is_within(),
            SpatialRelation::Contains => im.is_contains(),
            SpatialRelation::Intersects => im.is_intersects(),
            SpatialRelation::Disjoint => im.is_disjoint(),
            SpatialRelation::Touches => im.is_touches(),
            SpatialRelation::Crosses => im.is_crosses(),
            SpatialRelation::Overlaps => im.is_overlaps(),
            SpatialRelation::Equals => im.is_equal_topo(),
        })
    }

    /// Whether this is inside that. The relation a rule asks for most.
    pub fn within(&self, other: &Geometry) -> Option<bool> {
        self.relates(SpatialRelation::Within, other)
    }

    /// Great-circle distance in metres, **centre to centre**.
    ///
    /// A distance between two areas has to pick a definition, and this is the
    /// one chosen. The palette says "distance between centres" rather than
    /// "distance", so nobody has to infer which it was from the number.
    pub fn distance_to(&self, other: &Geometry) -> Option<f64> {
        use geo::{Distance, Haversine};
        let a = self.centre()?;
        let b = other.centre()?;
        Some(Haversine::distance(a, b))
    }

    fn centre(&self) -> Option<geo::Point<f64>> {
        use geo::Centroid;
        match &self.0 {
            geo::Geometry::Point(p) => Some(*p),
            other => other.centroid(),
        }
    }
}

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
        match g.0 {
            geo::Geometry::Point(p) => {
                // x is longitude, y is latitude — in that order.
                assert!((p.x() - -122.4194).abs() < 1e-9, "longitude is x");
                assert!((p.y() - 37.7749).abs() < 1e-9, "latitude is y");
            }
            other => panic!("expected a Point, got {other:?}"),
        }
    }

    #[test]
    fn every_geojson_geometry_type_parses() {
        for (name, json) in [
            ("Point", SF),
            (
                "LineString",
                r#"{"type":"LineString","coordinates":[[0,0],[1,1]]}"#,
            ),
            ("Polygon", BAY),
            (
                "MultiPoint",
                r#"{"type":"MultiPoint","coordinates":[[0,0],[1,1]]}"#,
            ),
            (
                "MultiLineString",
                r#"{"type":"MultiLineString","coordinates":[[[0,0],[1,1]]]}"#,
            ),
            (
                "MultiPolygon",
                r#"{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]}"#,
            ),
            (
                "GeometryCollection",
                r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[0,0]}]}"#,
            ),
        ] {
            assert!(Geometry::parse(json).is_some(), "{name} did not parse");
        }
    }

    #[test]
    fn a_feature_yields_its_geometry() {
        let f = format!(r#"{{"type":"Feature","geometry":{SF},"properties":{{"name":"office"}}}}"#);
        let g = Geometry::parse(&f).expect("a Feature yields its geometry");
        assert!(matches!(g.0, geo::Geometry::Point(_)));
    }

    #[test]
    fn something_that_is_not_geojson_is_refused_rather_than_guessed() {
        for bad in [
            "not json at all",
            r#"{"type":"Point"}"#,                   // no coordinates
            r#"{"type":"Point","coordinates":[1]}"#, // not a position
            r#"{"type":"Wormhole","coordinates":[0,0]}"#,
            r#"{"coordinates":[0,0]}"#, // no type
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
    fn a_point_on_the_boundary_is_not_within_but_does_intersect() {
        // DE-9IM's answer, not an invented one. `within` requires the
        // interiors to meet with nothing outside, and a boundary point fails
        // that; `intersects` and `touches` are how it is true. Following the
        // standard beats picking a side, because every other tool a person
        // compares against follows it too.
        let edge = Geometry::parse(r#"{"type":"Point","coordinates":[-123.0,37.5]}"#).unwrap();
        let bay = Geometry::parse(BAY).unwrap();
        assert_eq!(edge.within(&bay), Some(false));
        assert_eq!(edge.relates(SpatialRelation::Intersects, &bay), Some(true));
        assert_eq!(edge.relates(SpatialRelation::Touches, &bay), Some(true));
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

    // ── The rest of the spatial algebra ───────────────────────────

    #[test]
    fn a_polygon_can_be_within_another_polygon() {
        let inner = Geometry::parse(
            r#"{"type":"Polygon","coordinates":[[
                [-122.6,37.6],[-122.4,37.6],[-122.4,37.8],[-122.6,37.8],[-122.6,37.6]]]}"#,
        )
        .unwrap();
        let bay = Geometry::parse(BAY).unwrap();
        assert_eq!(inner.within(&bay), Some(true));
        assert_eq!(bay.within(&inner), Some(false));
    }

    #[test]
    fn the_de9im_relations_are_all_available() {
        let bay = Geometry::parse(BAY).unwrap();
        let sf = Geometry::parse(SF).unwrap();
        let nyc = Geometry::parse(NYC).unwrap();
        assert_eq!(bay.relates(SpatialRelation::Contains, &sf), Some(true));
        assert_eq!(sf.relates(SpatialRelation::Intersects, &bay), Some(true));
        assert_eq!(nyc.relates(SpatialRelation::Disjoint, &bay), Some(true));
        assert_eq!(sf.relates(SpatialRelation::Equals, &sf), Some(true));
    }

    #[test]
    fn distance_between_shapes_measures_between_their_centres() {
        // A distance between two areas has to pick a definition. Centre to
        // centre is the one chosen, and it is named that way in the palette
        // rather than left for someone to infer from a number.
        let bay = Geometry::parse(BAY).unwrap();
        let nyc = Geometry::parse(NYC).unwrap();
        let m = bay.distance_to(&nyc).expect("both are geometries");
        assert!(m > 3_000_000.0, "the bay is a long way from New York: {m}");
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
