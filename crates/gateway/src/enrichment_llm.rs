//! Classify LLM transport failures for entity enrichment UX (`provider_invalid` vs `failed`).

/// True when retrying the same provider settings is unlikely to help (auth, forbidden, wrong URL path).
/// Connection refused / timeouts stay false so the first attempt can still re-queue (see orchestrator retries).
pub fn is_enrichment_immediate_terminal_llm_error(message: &str) -> bool {
    let m = message.to_lowercase();
    if m.contains("401") && (m.contains("unauthorized") || m.contains("llm error")) {
        return true;
    }
    if m.contains("403") || m.contains("forbidden") {
        return true;
    }
    if m.contains("llm http error 404") || m.contains("llm http error 422") {
        return true;
    }
    false
}

/// Maps a failure message to `EntityEnrichment.status` after enrichment gives up.
pub fn enrichment_status_for_llm_error_message(message: &str) -> &'static str {
    let m = message.to_lowercase();
    // Auth/config-shaped errors: retrying with the same settings can't fix these.
    if m.contains("401")
        || m.contains("403")
        || m.contains("unauthorized")
        || m.contains("forbidden")
        || m.contains("llm http error 400")
        || m.contains("llm http error 401")
        || m.contains("llm http error 403")
        || m.contains("llm http error 404")
        || m.contains("llm http error 422")
        || m.contains("invalid api key")
        || m.contains("incorrect api key")
        || (m.contains("api key")
            && (m.contains("invalid") || m.contains("missing") || m.contains("required")))
    {
        return "provider_invalid";
    }
    // Connectivity-shaped errors: still mapped to provider_invalid here since
    // this function only runs after enrichment has already exhausted retries.
    if m.contains("failed to call")
        || m.contains("connection refused")
        || m.contains("connection reset")
        || m.contains("host unreachable")
        || m.contains("dns")
        || m.contains("timed out")
        || m.contains("timeout")
        || m.contains("could not connect")
        || m.contains("error sending request")
    {
        return "provider_invalid";
    }
    "failed"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_auth() {
        assert!(is_enrichment_immediate_terminal_llm_error(
            "LLM error 401 Unauthorized: nope"
        ));
        assert!(is_enrichment_immediate_terminal_llm_error(
            "LLM HTTP error 404: not found"
        ));
        assert!(!is_enrichment_immediate_terminal_llm_error(
            "failed to call OpenAI chat completions at http://127.0.0.1:1"
        ));
    }

    #[test]
    fn status_after_exhausted_retries() {
        assert_eq!(
            enrichment_status_for_llm_error_message("failed to call LLM at http://x"),
            "provider_invalid"
        );
        assert_eq!(
            enrichment_status_for_llm_error_message("could not parse JSON from model"),
            "failed"
        );
    }
}
