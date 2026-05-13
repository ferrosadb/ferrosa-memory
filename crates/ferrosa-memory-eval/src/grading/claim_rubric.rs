//! Claim-based rubric grader with anti-false-pass protection.
//!
//! Addresses EF01 (RPN 336) — the highest-risk failure mode in the FMEA.
//! Uses word-boundary regex matching with negation awareness to prevent
//! false positives from naive substring matching.

use regex::Regex;

/// Negation words that, when preceding a claim match, invalidate it.
/// Order matters: longer phrases first to avoid partial matches.
const NEGATION_WORDS: &[&str] = &["neither", "without", "never", "not", "no"];

/// Maximum number of characters before a match to scan for negation words.
const NEGATION_WINDOW: usize = 20;

/// Whether a claim asserts presence or absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimPolarity {
    /// The claim text must be found in the response.
    Positive,
    /// The claim text must NOT be found in the response (prefix: "NOT: ").
    Negative,
}

/// A single claim to evaluate against a response.
#[derive(Debug, Clone)]
pub struct Claim {
    /// Original claim text (without "NOT: " prefix for negative claims).
    pub text: String,
    /// Whether the claim asserts presence or absence.
    pub polarity: ClaimPolarity,
    /// Compiled word-boundary regex pattern.
    pub pattern: Regex,
}

/// Result of evaluating a single claim against a response.
#[derive(Debug, Clone)]
pub struct ClaimResult {
    /// The claim that was evaluated.
    pub claim_text: String,
    /// The polarity of the claim.
    pub polarity: ClaimPolarity,
    /// Whether the claim was satisfied.
    pub met: bool,
    /// Optional detail about why the claim failed.
    pub detail: Option<String>,
}

/// Aggregate score from evaluating all claims against a response.
#[derive(Debug, Clone)]
pub struct ClaimScore {
    /// Individual claim results.
    pub claims: Vec<ClaimResult>,
    /// Partial credit score: claims_met / total_claims (0.0 - 1.0).
    pub score: f64,
    /// Whether the score meets or exceeds the threshold.
    pub passed: bool,
    /// The passing threshold used.
    pub threshold: f64,
}

/// Result of a discrimination test — verifying claims fail against wrong responses.
#[derive(Debug, Clone)]
pub struct DiscriminationResult {
    /// Each entry: (wrong_response, score). All scores should be below threshold.
    pub results: Vec<(String, ClaimScore)>,
    /// True if ALL wrong responses scored below threshold (no false passes).
    pub all_discriminated: bool,
}

impl Claim {
    /// Parse a claim string, detecting "NOT: " prefix for negative polarity.
    ///
    /// # Errors
    /// Returns an error if the regex pattern cannot be compiled.
    pub fn parse(raw: &str) -> Result<Self, regex::Error> {
        let (text, polarity) = if let Some(stripped) = raw.strip_prefix("NOT: ") {
            (stripped.to_string(), ClaimPolarity::Negative)
        } else {
            (raw.to_string(), ClaimPolarity::Positive)
        };

        let escaped = regex::escape(&text);
        // Use \b only where the claim starts/ends with a word character.
        // Non-word characters (parentheses, dots, etc.) don't work with \b.
        let leading = if text.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            r"\b"
        } else {
            ""
        };
        let trailing = if text.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
            r"\b"
        } else {
            ""
        };
        let pattern_str = format!(r"(?i){}{}{}", leading, escaped, trailing);
        let pattern = Regex::new(&pattern_str)?;

        Ok(Self {
            text,
            polarity,
            pattern,
        })
    }

    /// Evaluate this claim against a response string.
    pub fn evaluate(&self, response: &str) -> ClaimResult {
        let raw_match = self.find_non_negated_match(response);

        let met = match self.polarity {
            ClaimPolarity::Positive => raw_match,
            ClaimPolarity::Negative => !self.pattern.is_match(response),
        };

        let detail = if !met {
            Some(match self.polarity {
                ClaimPolarity::Positive => {
                    if self.pattern.is_match(response) {
                        format!(
                            "Claim '{}' matched but was preceded by negation word",
                            self.text
                        )
                    } else {
                        format!("Claim '{}' not found in response", self.text)
                    }
                }
                ClaimPolarity::Negative => {
                    format!(
                        "Negative claim '{}' failed: text was found in response",
                        self.text
                    )
                }
            })
        } else {
            None
        };

        ClaimResult {
            claim_text: self.text.clone(),
            polarity: self.polarity,
            met,
            detail,
        }
    }

    /// Check if the claim pattern matches the response without being negated.
    ///
    /// For each regex match, inspects the preceding text (up to NEGATION_WINDOW chars)
    /// for negation words anywhere in the window at word boundaries.
    fn find_non_negated_match(&self, response: &str) -> bool {
        let lower_response = response.to_lowercase();

        for mat in self.pattern.find_iter(&lower_response) {
            let start = mat.start();
            let window_start = start.saturating_sub(NEGATION_WINDOW);
            let preceding = &lower_response[window_start..start];

            if !is_negated(preceding) {
                return true;
            }
        }

        false
    }
}

/// Check if the preceding text contains a negation word at a word boundary.
///
/// Scans the window for any negation word that appears as a standalone word
/// (at word boundaries). This catches negation even with intervening words,
/// e.g., "no entity was" negates whatever follows.
fn is_negated(preceding: &str) -> bool {
    let lower = preceding.to_lowercase();

    for &neg in NEGATION_WORDS {
        // Search for the negation word at word boundaries in the preceding text
        if let Ok(neg_pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(neg)))
            && neg_pattern.is_match(&lower)
        {
            return true;
        }
    }

    false
}

/// Grade a response against a set of claim strings.
///
/// Returns a `ClaimScore` with partial credit and pass/fail determination.
///
/// # Arguments
/// * `claim_strings` - Raw claim strings (may include "NOT: " prefix)
/// * `response` - The response text to evaluate
/// * `threshold` - Passing threshold (0.0 - 1.0)
///
/// # Errors
/// Returns an error if any claim string produces an invalid regex.
pub fn grade_claims(
    claim_strings: &[&str],
    response: &str,
    threshold: f64,
) -> Result<ClaimScore, regex::Error> {
    assert!(
        (0.0..=1.0).contains(&threshold),
        "threshold must be between 0.0 and 1.0"
    );

    let claims: Vec<Claim> = claim_strings
        .iter()
        .map(|s| Claim::parse(s))
        .collect::<Result<Vec<_>, _>>()?;

    let results: Vec<ClaimResult> = claims.iter().map(|c| c.evaluate(response)).collect();

    let met_count = results.iter().filter(|r| r.met).count();
    let total = results.len();
    let score = if total == 0 {
        0.0
    } else {
        met_count as f64 / total as f64
    };

    Ok(ClaimScore {
        claims: results,
        score,
        passed: score >= threshold,
        threshold,
    })
}

/// Run a discrimination test: verify that claims fail against known-wrong responses.
///
/// This addresses threat ET-E3 (trivially satisfiable claims). Every wrong response
/// must score below the threshold; if any passes, the claims are too lenient.
///
/// # Arguments
/// * `claim_strings` - Raw claim strings
/// * `wrong_responses` - Responses that should NOT satisfy the claims
/// * `threshold` - Passing threshold
///
/// # Errors
/// Returns an error if any claim string produces an invalid regex.
pub fn discrimination_test(
    claim_strings: &[&str],
    wrong_responses: &[&str],
    threshold: f64,
) -> Result<DiscriminationResult, regex::Error> {
    let mut results = Vec::with_capacity(wrong_responses.len());
    let mut all_discriminated = true;

    for &wrong in wrong_responses {
        let score = grade_claims(claim_strings, wrong, threshold)?;
        if score.passed {
            all_discriminated = false;
        }
        results.push((wrong.to_string(), score));
    }

    Ok(DiscriminationResult {
        results,
        all_discriminated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Claim parsing ──────────────────────────────────────────────

    #[test]
    fn test_parse_positive_claim() {
        let claim = Claim::parse("entity created").unwrap();
        assert_eq!(claim.text, "entity created");
        assert_eq!(claim.polarity, ClaimPolarity::Positive);
    }

    #[test]
    fn test_parse_negative_claim() {
        let claim = Claim::parse("NOT: error").unwrap();
        assert_eq!(claim.text, "error");
        assert_eq!(claim.polarity, ClaimPolarity::Negative);
    }

    #[test]
    fn test_parse_preserves_text_after_not_prefix() {
        let claim = Claim::parse("NOT: connection failed").unwrap();
        assert_eq!(claim.text, "connection failed");
        assert_eq!(claim.polarity, ClaimPolarity::Negative);
    }

    // ── Basic positive matching ────────────────────────────────────

    #[test]
    fn test_positive_claim_matches_present_text() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("The entity created successfully.");
        assert!(result.met, "Should match when text is present");
    }

    #[test]
    fn test_positive_claim_fails_when_absent() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("Something else happened.");
        assert!(!result.met, "Should not match when text is absent");
        assert!(result.detail.is_some());
    }

    #[test]
    fn test_positive_claim_case_insensitive() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("The ENTITY CREATED successfully.");
        assert!(result.met, "Should match case-insensitively");
    }

    // ── Word boundary matching (EF01 fix) ──────────────────────────

    #[test]
    fn test_word_boundary_prevents_partial_word_match() {
        // "entity_id" should NOT match a response containing only "session_id"
        let claim = Claim::parse("entity_id").unwrap();
        let result = claim.evaluate("The session_id was returned.");
        assert!(
            !result.met,
            "ET02: entity_id must not match session_id (word boundary)"
        );
    }

    #[test]
    fn test_word_boundary_allows_exact_match() {
        let claim = Claim::parse("entity_id").unwrap();
        let result = claim.evaluate("The entity_id was 12345.");
        assert!(result.met, "Should match entity_id exactly");
    }

    #[test]
    fn test_word_boundary_underscore_handling() {
        // Underscore is a word character, so \b matches at transitions
        let claim = Claim::parse("entity_id").unwrap();
        // "my_entity_id_value" should NOT match because it's embedded
        let result = claim.evaluate("my_entity_id_value was set.");
        assert!(
            !result.met,
            "entity_id should not match inside my_entity_id_value"
        );
    }

    // ── Negation awareness (ET01: the critical anti-false-pass test) ──

    #[test]
    fn test_et01_entity_created_does_not_match_no_entity_created() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("no entity created");
        assert!(
            !result.met,
            "ET01 CRITICAL: 'entity created' must NOT match 'no entity created'"
        );
    }

    #[test]
    fn test_negation_not_prefix() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("not entity created in this run");
        assert!(
            !result.met,
            "'entity created' must not match 'not entity created'"
        );
    }

    #[test]
    fn test_negation_never_prefix() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("never entity created here");
        assert!(
            !result.met,
            "'entity created' must not match 'never entity created'"
        );
    }

    #[test]
    fn test_negation_without_prefix() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("without entity created");
        assert!(
            !result.met,
            "'entity created' must not match 'without entity created'"
        );
    }

    #[test]
    fn test_negation_neither_prefix() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("neither entity created nor deleted");
        assert!(
            !result.met,
            "'entity created' must not match 'neither entity created'"
        );
    }

    #[test]
    fn test_negation_does_not_block_legitimate_match() {
        // "entity created" should still match when not negated
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("The entity created successfully.");
        assert!(result.met, "Should match when not preceded by negation");
    }

    #[test]
    fn test_negation_word_must_be_at_word_boundary() {
        // "ano entity created" — "no" is part of "ano", not a standalone negation
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("ano entity created");
        assert!(
            result.met,
            "'no' inside 'ano' is not a negation word at a word boundary"
        );
    }

    #[test]
    fn test_negation_with_extra_whitespace() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("no  entity created");
        assert!(
            !result.met,
            "Negation with extra whitespace should still be caught"
        );
    }

    #[test]
    fn test_negation_mixed_with_non_negated_match() {
        // If response has both "no entity created" AND a non-negated "entity created",
        // the claim SHOULD match (the non-negated occurrence satisfies it).
        let claim = Claim::parse("entity created").unwrap();
        let result =
            claim.evaluate("no entity created initially, but then entity created successfully");
        assert!(
            result.met,
            "Should match the non-negated occurrence in the same response"
        );
    }

    // ── Negative claims (ET03) ─────────────────────────────────────

    #[test]
    fn test_et03_negative_claim_fails_when_text_present() {
        let claim = Claim::parse("NOT: error").unwrap();
        let result = claim.evaluate("An error occurred during processing.");
        assert!(
            !result.met,
            "ET03: Negative claim 'NOT: error' must fail when 'error' is in response"
        );
    }

    #[test]
    fn test_negative_claim_passes_when_text_absent() {
        let claim = Claim::parse("NOT: error").unwrap();
        let result = claim.evaluate("Operation completed successfully.");
        assert!(
            result.met,
            "Negative claim should pass when the text is absent"
        );
    }

    #[test]
    fn test_negative_claim_word_boundary() {
        // "NOT: error" should NOT be triggered by "erroneous" (word boundary)
        let claim = Claim::parse("NOT: error").unwrap();
        let result = claim.evaluate("No erroneous behavior detected.");
        assert!(
            result.met,
            "Negative claim should use word boundaries: 'error' != 'erroneous'"
        );
    }

    // ── Partial credit scoring ─────────────────────────────────────

    #[test]
    fn test_partial_credit_all_claims_met() {
        let claims = vec!["entity created", "NOT: error"];
        let response = "The entity created successfully with no issues.";
        let score = grade_claims(&claims, response, 0.75).unwrap();
        assert_eq!(score.score, 1.0);
        assert!(score.passed);
    }

    #[test]
    fn test_partial_credit_half_claims_met() {
        let claims = vec!["entity created", "session started"];
        let response = "The entity created successfully.";
        let score = grade_claims(&claims, response, 0.75).unwrap();
        assert_eq!(score.score, 0.5);
        assert!(!score.passed, "0.5 < 0.75 threshold");
    }

    #[test]
    fn test_partial_credit_no_claims_met() {
        let claims = vec!["entity created", "session started"];
        let response = "Nothing relevant here.";
        let score = grade_claims(&claims, response, 0.75).unwrap();
        assert_eq!(score.score, 0.0);
        assert!(!score.passed);
    }

    #[test]
    fn test_threshold_boundary_exact() {
        let claims = vec!["alpha", "beta", "gamma", "delta"];
        let response = "alpha beta gamma something";
        let score = grade_claims(&claims, response, 0.75).unwrap();
        assert_eq!(score.score, 0.75);
        assert!(score.passed, "Score exactly at threshold should pass");
    }

    #[test]
    fn test_empty_claims_returns_zero() {
        let claims: Vec<&str> = vec![];
        let score = grade_claims(&claims, "any response", 0.5).unwrap();
        assert_eq!(score.score, 0.0);
        assert!(!score.passed);
    }

    // ── Discrimination test (ET-E3: trivially satisfiable claims) ──

    #[test]
    fn test_discrimination_all_wrong_responses_fail() {
        let claims = vec![
            "entity created",
            "similarity score above threshold",
            "NOT: error",
        ];

        let wrong_responses = vec![
            "no entity created at all",
            "session_id returned but nothing else",
            "error: connection refused",
            "the request was not entity created because of timeout",
        ];

        let result = discrimination_test(&claims, &wrong_responses, 0.75).unwrap();
        assert!(
            result.all_discriminated,
            "All wrong responses must score below threshold. Results: {:?}",
            result
                .results
                .iter()
                .map(|(r, s)| format!("'{}' => {}", r, s.score))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_discrimination_detects_overly_lenient_claims() {
        // Intentionally lenient claim that matches almost anything
        let claims = vec!["the"];

        let wrong_responses = vec!["the cat sat on the mat"];

        let result = discrimination_test(&claims, &wrong_responses, 0.5).unwrap();
        assert!(
            !result.all_discriminated,
            "Overly lenient claims should fail discrimination"
        );
    }

    // ── Adversarial test suite (0% false positive rate) ────────────

    #[test]
    fn test_adversarial_zero_false_positive_rate() {
        // Comprehensive adversarial suite: each entry is (claim, adversarial_response)
        // where the claim MUST NOT match the response.
        let adversarial_cases: Vec<(&str, &str)> = vec![
            // ET01: negation before match
            ("entity created", "no entity created"),
            ("entity created", "not entity created"),
            ("entity created", "never entity created"),
            ("entity created", "without entity created"),
            ("entity created", "neither entity created nor deleted"),
            // ET02: cross-field false match
            ("entity_id", "session_id was returned"),
            ("entity_id", "the user_entity_identifier was set"),
            // Substring traps
            ("created", "no entity was created"),
            ("success", "not a success"),
            // Negative claims with text present
            ("NOT: error", "an error occurred"),
            ("NOT: failed", "the operation failed"),
            ("NOT: timeout", "connection timeout detected"),
        ];

        let mut false_positives = 0;
        let mut failures = Vec::new();

        for (claim_str, response) in &adversarial_cases {
            let claim = Claim::parse(claim_str).unwrap();
            let result = claim.evaluate(response);
            if result.met {
                false_positives += 1;
                failures.push(format!(
                    "FALSE POSITIVE: claim='{}' matched response='{}'",
                    claim_str, response
                ));
            }
        }

        assert_eq!(
            false_positives,
            0,
            "0% false positive rate required. Failures:\n{}",
            failures.join("\n")
        );
    }

    // ── Integration: grade_claims with mixed polarity ──────────────

    #[test]
    fn test_grade_claims_mixed_polarity() {
        let claims = vec![
            "entity created",
            "similarity score",
            "NOT: error",
            "NOT: timeout",
        ];

        let response = "The entity created with a similarity score of 0.95.";
        let score = grade_claims(&claims, response, 0.75).unwrap();
        assert_eq!(score.score, 1.0);
        assert!(score.passed);
        assert_eq!(score.claims.len(), 4);
    }

    #[test]
    fn test_grade_claims_negative_claim_failure_reduces_score() {
        let claims = vec!["entity created", "NOT: error"];
        let response = "The entity created but an error occurred.";
        let score = grade_claims(&claims, response, 0.75).unwrap();
        assert_eq!(score.score, 0.5, "One of two claims failed");
        assert!(!score.passed);
    }

    // ── Edge cases ─────────────────────────────────────────────────

    #[test]
    fn test_special_regex_characters_in_claim() {
        // Parentheses, brackets, etc. must be escaped
        let claim = Claim::parse("score (0.95)").unwrap();
        let result = claim.evaluate("The score (0.95) was returned.");
        assert!(result.met, "Special regex chars should be escaped");
    }

    #[test]
    fn test_claim_with_numbers() {
        let claim = Claim::parse("entity_id 12345").unwrap();
        let result = claim.evaluate("Found entity_id 12345 in the database.");
        assert!(result.met);
    }

    #[test]
    fn test_negation_case_insensitive() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("No entity created at all.");
        assert!(!result.met, "Negation detection should be case-insensitive");
    }

    #[test]
    fn test_claim_result_detail_on_negated_match() {
        let claim = Claim::parse("entity created").unwrap();
        let result = claim.evaluate("no entity created");
        assert!(!result.met);
        assert!(result.detail.is_some());
        let detail = result.detail.unwrap();
        assert!(
            detail.contains("negation"),
            "Detail should mention negation: {}",
            detail
        );
    }

    #[test]
    fn test_claim_result_detail_on_negative_claim_failure() {
        let claim = Claim::parse("NOT: error").unwrap();
        let result = claim.evaluate("An error happened.");
        assert!(!result.met);
        assert!(result.detail.is_some());
        let detail = result.detail.unwrap();
        assert!(
            detail.contains("Negative claim"),
            "Detail should reference negative claim: {}",
            detail
        );
    }

    #[test]
    fn test_grade_claims_preserves_claim_order() {
        let claims = vec!["alpha", "beta", "gamma"];
        let response = "gamma beta alpha";
        let score = grade_claims(&claims, response, 0.5).unwrap();
        assert_eq!(score.claims[0].claim_text, "alpha");
        assert_eq!(score.claims[1].claim_text, "beta");
        assert_eq!(score.claims[2].claim_text, "gamma");
    }

    #[test]
    #[should_panic(expected = "threshold must be between")]
    fn test_grade_claims_rejects_invalid_threshold() {
        let _ = grade_claims(&["test"], "response", 1.5);
    }
}
