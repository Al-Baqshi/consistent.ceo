//! Which saved logins belong to a web address.
//!
//! The rule is the one an entity's browser uses for the sites it may visit: a
//! login saved for `instagram.com` is offered on `instagram.com` and on its
//! subdomains (`www.instagram.com`, `help.instagram.com`), and never on a domain
//! that only looks alike (`instagram.com.evil.example`, `notinstagram.com`).
//! `www.` is ignored on both sides. One rule for both means the wallet and an
//! entity's browser agree on what a site is.
//!
//! It is deliberately the conservative direction. A login saved on
//! `login.example.com` is not offered on `www.example.com`: telling which
//! domains belong together needs the public suffix list (`a.co.uk` and
//! `b.co.uk` are strangers), and guessing wrong would offer one site's password
//! to another. Saving a second address on the login covers that case.
//!
//! Hosts are compared, not whole addresses: a login saved as
//! `https://www.instagram.com/accounts/login/?next=/` is a login for
//! `instagram.com`.

use serde::Serialize;

/// The host a saved URI or a page address names, ready to compare: lowercase,
/// without `www.`, a trailing dot, a port or credentials. `None` for anything
/// that is not a web address (`androidapp://…`, `mailto:`, an empty string).
/// A bare domain (`instagram.com/login`) counts, since that is how people type
/// them.
pub fn site_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let rest = match raw.split_once("://") {
        Some((scheme, rest)) => {
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                return None;
            }
            rest
        }
        None => {
            // `mailto:x`, `otpauth:…`: a scheme without `//`. A domain always has
            // a dot before any colon (`example.com:8080`), a scheme never does.
            if let Some((before, _)) = raw.split_once(':')
                && !before.contains('.')
            {
                return None;
            }
            raw
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => host_port,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let valid = host.contains('.')
        && !host.starts_with('.')
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    valid.then_some(host)
}

/// How well a saved address fits a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SiteMatch {
    /// The page is on a subdomain of the saved site (`help.instagram.com`).
    Subdomain,
    /// The page is on the saved site itself.
    Host,
}

/// Whether a login saved for `saved` belongs on a page whose host is
/// `page_host` (as [`site_host`] returns it).
pub fn site_match(saved: &str, page_host: &str) -> Option<SiteMatch> {
    let saved = site_host(saved)?;
    if page_host == saved {
        Some(SiteMatch::Host)
    } else if page_host.ends_with(&format!(".{saved}")) {
        Some(SiteMatch::Subdomain)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_addresses_come_down_to_their_site() {
        for (saved, host) in [
            ("https://www.instagram.com/accounts/login/?next=/", "instagram.com"),
            ("instagram.com", "instagram.com"),
            ("Instagram.COM/login", "instagram.com"),
            ("http://me:pw@studio.example.co.nz:8443/x", "studio.example.co.nz"),
            ("https://x.com.", "x.com"),
            ("  https://www.linkedin.com  ", "linkedin.com"),
            ("example.com:8080", "example.com"),
        ] {
            assert_eq!(site_host(saved).as_deref(), Some(host), "{saved}");
        }
    }

    #[test]
    fn what_is_not_a_web_address_has_no_site() {
        for saved in [
            "",
            "   ",
            "androidapp://com.instagram.android",
            "mailto:owner@example.com",
            "otpauth://totp/x",
            "ftp://files.example.com",
            "localhost",
            "not a site",
            "https://",
            "https://a..b.com",
        ] {
            assert_eq!(site_host(saved), None, "{saved:?}");
        }
    }

    #[test]
    fn a_login_is_offered_on_its_site_and_its_subdomains_only() {
        let page = |url: &str| site_host(url).unwrap();
        let saved = "https://instagram.com";
        assert_eq!(site_match(saved, &page("https://www.instagram.com/")), Some(SiteMatch::Host));
        assert_eq!(
            site_match(saved, &page("https://help.instagram.com/x")),
            Some(SiteMatch::Subdomain)
        );
        // Look-alikes and strangers never match.
        for stranger in [
            "https://notinstagram.com",
            "https://instagram.com.evil.example",
            "https://instagram.co",
            "https://facebook.com",
        ] {
            assert_eq!(site_match(saved, &page(stranger)), None, "{stranger}");
        }
        // The conservative direction: a login for a subdomain is not offered on its parent.
        assert_eq!(site_match("login.example.com", &page("https://example.com")), None);
        // A saved address that is not a web address matches nothing.
        assert_eq!(site_match("androidapp://x", &page("https://x.com")), None);
    }

    #[test]
    fn the_site_itself_ranks_above_a_subdomain() {
        assert!(SiteMatch::Host > SiteMatch::Subdomain);
    }
}
