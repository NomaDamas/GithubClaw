//! GitHub HTML marker parser for inter-agent state transitions.
//!
//! All agent-to-system communication uses HTML comments in GitHub issue/PR comments.
//! This module provides deterministic parsing of these markers.
//!
//! Marker format: `<!-- githubclaw:<marker_type> [key=value ...] -->`
//! Summary format: `<!-- githubclaw:summary -->...<!-- /githubclaw:summary -->`

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Marker types
// ---------------------------------------------------------------------------

/// Known marker types that trigger state transitions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerType {
    Approved,
    Reproduced,
    Reviewed,
    Verified,
    Stuck,
}

impl MarkerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Reproduced => "reproduced",
            Self::Reviewed => "reviewed",
            Self::Verified => "verified",
            Self::Stuck => "stuck",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "approved" => Some(Self::Approved),
            "reproduced" => Some(Self::Reproduced),
            "reviewed" => Some(Self::Reviewed),
            "verified" => Some(Self::Verified),
            "stuck" => Some(Self::Stuck),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsed marker
// ---------------------------------------------------------------------------

/// A parsed marker extracted from a GitHub comment body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedMarker {
    pub marker_type: MarkerType,
    pub attributes: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Regex patterns (compiled once)
// ---------------------------------------------------------------------------

static MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<!--\s*githubclaw:(\w+)((?:\s+\w+=\S+)*)\s*-->").unwrap()
});

static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\w+)=(\S+)").unwrap()
});

static SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<!--\s*githubclaw:summary\s*-->(.*?)<!--\s*/githubclaw:summary\s*-->"
    )
    .unwrap()
});

static REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"ref\s+#(\d+)").unwrap()
});

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse all known markers from a GitHub comment body.
///
/// Returns a vec of parsed markers. Unknown marker types are silently skipped.
pub fn parse_markers(comment_body: &str) -> Vec<ParsedMarker> {
    let mut markers = Vec::new();

    for cap in MARKER_RE.captures_iter(comment_body) {
        let marker_str = &cap[1];
        let attrs_str = cap.get(2).map(|m| m.as_str()).unwrap_or("");

        if let Some(marker_type) = MarkerType::from_str(marker_str) {
            let mut attributes = HashMap::new();
            for attr_cap in ATTR_RE.captures_iter(attrs_str) {
                attributes.insert(attr_cap[1].to_string(), attr_cap[2].to_string());
            }
            markers.push(ParsedMarker {
                marker_type,
                attributes,
            });
        }
    }

    markers
}

/// Extract the summary block from a GitHub comment body.
///
/// Returns `None` if no summary block is found.
pub fn extract_summary(comment_body: &str) -> Option<String> {
    SUMMARY_RE
        .captures(comment_body)
        .map(|cap| cap[1].trim().to_string())
}

/// Extract the root issue reference (`ref #N`) from a GitHub comment body.
///
/// Returns `None` if no ref is found.
pub fn extract_ref_issue(comment_body: &str) -> Option<u64> {
    REF_RE
        .captures(comment_body)
        .and_then(|cap| cap[1].parse::<u64>().ok())
}

/// Generate a marker HTML comment string.
pub fn format_marker(marker_type: &MarkerType, attributes: &HashMap<String, String>) -> String {
    let attrs: String = attributes
        .iter()
        .map(|(k, v)| format!(" {}={}", k, v))
        .collect();
    format!("<!-- githubclaw:{}{} -->", marker_type.as_str(), attrs)
}

/// Generate a summary block.
pub fn format_summary(content: &str) -> String {
    format!(
        "<!-- githubclaw:summary -->\n{}\n<!-- /githubclaw:summary -->",
        content
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // 1. Parse simple approved marker
    #[test]
    fn parse_approved_marker() {
        let body = "Some text\n<!-- githubclaw:approved -->\nMore text";
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].marker_type, MarkerType::Approved);
        assert!(markers[0].attributes.is_empty());
    }

    // 2. Parse marker with attributes
    #[test]
    fn parse_reproduced_with_attributes() {
        let body = "<!-- githubclaw:reproduced reproduced=true os=linux -->";
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].marker_type, MarkerType::Reproduced);
        assert_eq!(markers[0].attributes["reproduced"], "true");
        assert_eq!(markers[0].attributes["os"], "linux");
    }

    // 3. Parse multiple markers
    #[test]
    fn parse_multiple_markers() {
        let body = "<!-- githubclaw:approved -->\nSome text\n<!-- githubclaw:verified -->";
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].marker_type, MarkerType::Approved);
        assert_eq!(markers[1].marker_type, MarkerType::Verified);
    }

    // 4. Unknown markers are skipped
    #[test]
    fn unknown_markers_skipped() {
        let body = "<!-- githubclaw:unknown_type -->\n<!-- githubclaw:approved -->";
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].marker_type, MarkerType::Approved);
    }

    // 5. No markers returns empty
    #[test]
    fn no_markers_returns_empty() {
        let body = "Just a regular comment with no markers.";
        let markers = parse_markers(body);
        assert!(markers.is_empty());
    }

    // 6. Extract summary block
    #[test]
    fn extract_summary_block() {
        let body = "Header\n<!-- githubclaw:summary -->\nBug was reproduced on Ubuntu 22.04.\nRoot cause: null pointer in auth module.\n<!-- /githubclaw:summary -->\nFooter";
        let summary = extract_summary(body);
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("Bug was reproduced"));
        assert!(s.contains("Root cause"));
    }

    // 7. No summary returns None
    #[test]
    fn no_summary_returns_none() {
        let body = "Just text, no summary block.";
        assert!(extract_summary(body).is_none());
    }

    // 8. Extract ref issue number
    #[test]
    fn extract_ref_issue_number() {
        let body = "Fixed the bug.\n\nref #123";
        assert_eq!(extract_ref_issue(body), Some(123));
    }

    // 9. No ref returns None
    #[test]
    fn no_ref_returns_none() {
        let body = "No reference here.";
        assert!(extract_ref_issue(body).is_none());
    }

    // 10. Format marker
    #[test]
    fn format_marker_simple() {
        let marker = format_marker(&MarkerType::Approved, &HashMap::new());
        assert_eq!(marker, "<!-- githubclaw:approved -->");
    }

    // 11. Format marker with attributes
    #[test]
    fn format_marker_with_attrs() {
        let mut attrs = HashMap::new();
        attrs.insert("reproduced".into(), "true".into());
        let marker = format_marker(&MarkerType::Reproduced, &attrs);
        assert!(marker.contains("githubclaw:reproduced"));
        assert!(marker.contains("reproduced=true"));
    }

    // 12. Format summary
    #[test]
    fn format_summary_block() {
        let summary = format_summary("Test passed on all platforms.");
        assert!(summary.contains("<!-- githubclaw:summary -->"));
        assert!(summary.contains("Test passed on all platforms."));
        assert!(summary.contains("<!-- /githubclaw:summary -->"));
    }

    // 13. Roundtrip: format then parse marker
    #[test]
    fn roundtrip_format_parse_marker() {
        let mut attrs = HashMap::new();
        attrs.insert("reproduced".into(), "false".into());
        let formatted = format_marker(&MarkerType::Reproduced, &attrs);
        let parsed = parse_markers(&formatted);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].marker_type, MarkerType::Reproduced);
        assert_eq!(parsed[0].attributes["reproduced"], "false");
    }

    // 14. Roundtrip: format then extract summary
    #[test]
    fn roundtrip_format_extract_summary() {
        let content = "E2E test passed. All endpoints responding correctly.";
        let formatted = format_summary(content);
        let extracted = extract_summary(&formatted).unwrap();
        assert_eq!(extracted, content);
    }

    // 15. Parse stuck marker
    #[test]
    fn parse_stuck_marker() {
        let body = "<!-- githubclaw:stuck loop=implementer count=10 -->";
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].marker_type, MarkerType::Stuck);
        assert_eq!(markers[0].attributes["loop"], "implementer");
        assert_eq!(markers[0].attributes["count"], "10");
    }

    // 16. Marker with extra whitespace
    #[test]
    fn marker_with_extra_whitespace() {
        let body = "<!--   githubclaw:verified   -->";
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].marker_type, MarkerType::Verified);
    }

    // 17. MarkerType as_str and from_str roundtrip
    #[test]
    fn marker_type_roundtrip() {
        let types = vec![
            MarkerType::Approved,
            MarkerType::Reproduced,
            MarkerType::Reviewed,
            MarkerType::Verified,
            MarkerType::Stuck,
        ];
        for mt in types {
            assert_eq!(MarkerType::from_str(mt.as_str()), Some(mt));
        }
    }

    // 18. Complex comment with markers, summary, and ref
    #[test]
    fn complex_comment_full_parse() {
        let body = r#"## Bug Reproduction Report

<!-- githubclaw:reproduced reproduced=true -->

<!-- githubclaw:summary -->
Bug confirmed on Ubuntu 22.04 in Docker container.
Steps: 1) Install deps 2) Run `cargo test` 3) Observe panic at line 42.
Root cause: integer overflow in rate_limiter.rs
<!-- /githubclaw:summary -->

ref #456
"#;
        let markers = parse_markers(body);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].marker_type, MarkerType::Reproduced);
        assert_eq!(markers[0].attributes["reproduced"], "true");

        let summary = extract_summary(body).unwrap();
        assert!(summary.contains("Bug confirmed on Ubuntu"));
        assert!(summary.contains("integer overflow"));

        assert_eq!(extract_ref_issue(body), Some(456));
    }
}
