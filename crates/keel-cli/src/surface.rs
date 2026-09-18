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
}
