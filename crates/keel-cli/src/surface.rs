//! Which Google generative-AI surface a host belongs to.
//!
//! Google serves generative AI over two surfaces that share one SDK and
//! therefore one Keel target name (`llm:google-genai`): Vertex AI and the
//! Gemini Developer API. They differ in host, auth, and operation-read shape,
//! so a `poll` route key written for one is wrong for the other. This module
//! is the single place that decision is made from a host string.
//!
//! Deliberately host-only: the richer signals (SDK construction kwargs, env
//! var names) are out of scope — see the spec's D3.

// Consumed starting in a later task of this program (route-key inference);
// this task lays the classifier down on its own.
#![allow(dead_code)]

use std::collections::BTreeSet;

use keel_core_api::policy::VERTEX_REGIONAL_SUFFIX;
use serde::Serialize;

/// One Google generative-AI surface.
///
/// `Ord` follows declaration order, so a sorted collection is deterministic
/// for golden output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Surface {
    GeminiApi,
    Vertex,
}

/// The surface a host belongs to, or `None` for any host that is not a Google
/// generative-AI host.
pub(crate) fn classify_host(host: &str) -> Option<Surface> {
    if host == "aiplatform.googleapis.com" || host.ends_with(VERTEX_REGIONAL_SUFFIX) {
        return Some(Surface::Vertex);
    }
    if host == "generativelanguage.googleapis.com" {
        return Some(Surface::GeminiApi);
    }
    None
}

/// Which Google surfaces a project uses, and the evidence that decided it.
#[derive(Debug, Serialize)]
pub(crate) struct SurfaceEvidence {
    /// Sorted, deduped. Empty when nothing Google-shaped was found.
    pub(crate) detected: Vec<Surface>,
    /// The strongest signal that contributed: `"policy" > "static"`, or
    /// `"none"` when `detected` is empty.
    ///
    /// There is deliberately no `"runtime"` variant. Keel discards the host at
    /// target resolution, so it never reaches the discovery store or the
    /// events feed and cannot be read back (#140). Adding one later is
    /// additive for consumers.
    pub(crate) source: &'static str,
    /// The hosts that drove the decision, sorted and deduped.
    pub(crate) evidence: Vec<String>,
}

/// Detect the surface set from the two static signals, unioned.
///
/// `policy_hosts` are hosts named by the project's own `keel.toml`;
/// `scanner_hosts` are host literals the scanner sighted. Hosts that are not
/// Google generative-AI hosts contribute nothing to either.
pub(crate) fn detect_surfaces(
    policy_hosts: &[String],
    scanner_hosts: &[String],
) -> SurfaceEvidence {
    let mut detected: BTreeSet<Surface> = BTreeSet::new();
    let mut evidence: BTreeSet<String> = BTreeSet::new();
    let mut from_policy = false;

    for (hosts, is_policy) in [(policy_hosts, true), (scanner_hosts, false)] {
        for h in hosts {
            if let Some(surface) = classify_host(h) {
                detected.insert(surface);
                evidence.insert(h.clone());
                from_policy |= is_policy;
            }
        }
    }

    let source = if detected.is_empty() {
        "none"
    } else if from_policy {
        "policy"
    } else {
        "static"
    };

    SurfaceEvidence {
        detected: detected.into_iter().collect(),
        source,
        evidence: evidence.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_both_google_surfaces_and_ignores_everything_else() {
        assert_eq!(
            classify_host("aiplatform.googleapis.com"),
            Some(Surface::Vertex)
        );
        assert_eq!(
            classify_host("us-central1-aiplatform.googleapis.com"),
            Some(Surface::Vertex)
        );
        assert_eq!(
            classify_host("generativelanguage.googleapis.com"),
            Some(Surface::GeminiApi)
        );
        assert_eq!(classify_host("api.openai.com"), None);
        assert_eq!(classify_host("api.anthropic.com"), None);
        assert_eq!(classify_host("example.com"), None);
    }

    #[test]
    fn a_host_merely_containing_the_suffix_is_not_vertex() {
        // Guards against `contains` instead of `ends_with`.
        assert_eq!(classify_host("aiplatform.googleapis.com.evil.test"), None);
    }

    #[test]
    fn surface_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Surface::GeminiApi).unwrap(),
            "\"gemini-api\""
        );
        assert_eq!(
            serde_json::to_string(&Surface::Vertex).unwrap(),
            "\"vertex\""
        );
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn policy_alone_decides_and_is_named_as_the_source() {
        let got = detect_surfaces(&s(&["us-central1-aiplatform.googleapis.com"]), &[]);
        assert_eq!(got.detected, vec![Surface::Vertex]);
        assert_eq!(got.source, "policy");
        assert_eq!(got.evidence, s(&["us-central1-aiplatform.googleapis.com"]));
    }

    #[test]
    fn topology_alone_decides_and_is_named_as_the_source() {
        let got = detect_surfaces(&[], &s(&["generativelanguage.googleapis.com"]));
        assert_eq!(got.detected, vec![Surface::GeminiApi]);
        assert_eq!(got.source, "static");
    }

    #[test]
    fn both_signals_union_and_policy_wins_the_source_label() {
        // They disagree: the union is BOTH, which is the honest answer when
        // only static evidence exists.
        let got = detect_surfaces(
            &s(&["us-central1-aiplatform.googleapis.com"]),
            &s(&["generativelanguage.googleapis.com"]),
        );
        assert_eq!(got.detected, vec![Surface::GeminiApi, Surface::Vertex]);
        assert_eq!(got.source, "policy");
        assert_eq!(
            got.evidence,
            s(&[
                "generativelanguage.googleapis.com",
                "us-central1-aiplatform.googleapis.com"
            ])
        );
    }

    #[test]
    fn no_google_hosts_yields_an_empty_set_and_none() {
        let got = detect_surfaces(&s(&["api.openai.com"]), &s(&["example.com"]));
        assert!(got.detected.is_empty());
        assert_eq!(got.source, "none");
        assert!(got.evidence.is_empty());
    }

    #[test]
    fn duplicate_hosts_collapse() {
        let got = detect_surfaces(
            &s(&["aiplatform.googleapis.com"]),
            &s(&[
                "aiplatform.googleapis.com",
                "us-east1-aiplatform.googleapis.com",
            ]),
        );
        assert_eq!(got.detected, vec![Surface::Vertex]);
        assert_eq!(
            got.evidence,
            s(&[
                "aiplatform.googleapis.com",
                "us-east1-aiplatform.googleapis.com"
            ])
        );
    }
}
