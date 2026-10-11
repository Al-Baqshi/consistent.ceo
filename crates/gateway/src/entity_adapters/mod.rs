//! Real connection adapters: domain HTTP verify, SendGrid email, Twitter bearer test.

mod domain;
mod email;
mod social;

pub use domain::{domain_from_config, verify_domain_ownership};
pub use email::{send_email_via_adapter, test_sendgrid_key};
pub use social::test_social_bearer;

/// Single place to normalize adapter/provider errors for UI toasts and `last_error`.
pub fn user_facing_adapter_error(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("invalid credentials")
    {
        return "Authentication failed — reconnect or rotate credentials (reauth required)."
            .to_string();
    }
    if lower.contains("404") || lower.contains("not found") {
        return "Resource not found — check domain, token path, or provider configuration."
            .to_string();
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return "Network timeout — try again or check connectivity.".to_string();
    }
    // Cap the surfaced message so a large provider error body doesn't blow up
    // a UI toast or `last_error` field.
    let t = raw.trim();
    if t.len() > 280 {
        // Cut on a char boundary: provider errors are often localized, and a
        // byte slice through a multi-byte char panics.
        format!("{}…", &t[..t.floor_char_boundary(280)])
    } else {
        t.to_string()
    }
}
