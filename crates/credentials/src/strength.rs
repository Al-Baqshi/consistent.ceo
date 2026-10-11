//! Master-password strength, estimated the way an attacker's cracker actually
//! works rather than by counting character classes.
//!
//! A rule like "8 characters, one number, one symbol" cheerfully passes
//! `P@ssw0rd!` and fails `correct horse battery staple`, which is exactly
//! backwards. [`zxcvbn`] instead models guessing cost against dictionaries,
//! keyboard walks, repeats, sequences and l33t substitutions, and reports an
//! order-of-magnitude guess count. That number is what matters, because the
//! master password is the one secret with no second factor behind it — every
//! other credential in the vault is only as safe as this one.
//!
//! This is advisory at creation, never a hard gate beyond a floor. A user who
//! insists on a weak master password is making an informed choice about their own
//! vault; the job here is to make sure it is informed.

use serde::Serialize;

/// The shortest master password allowed at all. Below this, creation is refused
/// outright rather than merely warned about — there is no legitimate reason for a
/// vault master password to be shorter, and it is the one place a hard floor is
/// justified.
pub const MIN_MASTER_PASSWORD_LEN: usize = 8;

/// A strength assessment suitable for showing a meter and a sentence of guidance.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Strength {
    /// zxcvbn's 0–4 score. 0 is trivially cracked; 4 is strong.
    pub score: u8,
    /// A short human label for the score.
    pub label: &'static str,
    /// Whether this clears the bar we would encourage for a master password
    /// (score >= 3). Not a hard gate; [`MIN_MASTER_PASSWORD_LEN`] is.
    pub acceptable: bool,
    /// The single most useful next improvement, if any. Straight from zxcvbn's
    /// own feedback so the advice is specific ("add another word") rather than
    /// generic ("make it stronger").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// A caution about *why* this password is weak, when zxcvbn has one — e.g.
    /// "this is a top-100 password" or "dates are easy to guess".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Order-of-magnitude guesses to crack, as a display string like "10^12".
    /// Communicates the stakes better than a 0–4 score alone.
    pub guesses_log10: String,
}

// Maps zxcvbn's 0-4 score to the label shown alongside it.
fn label_for(score: u8) -> &'static str {
    match score {
        0 => "Very weak",
        1 => "Weak",
        2 => "Fair",
        3 => "Strong",
        _ => "Very strong",
    }
}

/// Estimate the strength of a candidate master password.
///
/// `context` holds strings that should count as weak *for this user
/// specifically* — their email, name, workspace — because a password built from
/// them is guessable by anyone who knows them, however random it looks to a
/// generic dictionary.
pub fn estimate(password: &str, context: &[&str]) -> Strength {
    let entropy = zxcvbn::zxcvbn(password, context);
    let score = u8::from(entropy.score());

    let feedback = entropy.feedback();
    let suggestion = feedback
        .and_then(|f| f.suggestions().first())
        .map(|s| s.to_string());
    let warning = feedback.and_then(|f| f.warning()).map(|w| w.to_string());

    // guesses() can be enormous; log10 keeps it to a small, comparable number.
    let guesses_log10 = format!("10^{:.0}", entropy.guesses_log10());

    Strength {
        score,
        label: label_for(score),
        acceptable: score >= 3,
        suggestion,
        warning,
        guesses_log10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_classic_bad_password_scores_low_despite_looking_complex() {
        // Passes every character-class rule; zxcvbn is not fooled.
        let s = estimate("P@ssw0rd!", &[]);
        assert!(s.score <= 1, "got score {} for P@ssw0rd!", s.score);
        assert!(!s.acceptable);
    }

    #[test]
    fn a_long_passphrase_scores_high_despite_being_all_lowercase() {
        // The case a character-class rule wrongly rejects.
        let s = estimate("correct horse battery staple", &[]);
        assert!(
            s.score >= 3,
            "got score {} for a four-word passphrase",
            s.score
        );
        assert!(s.acceptable);
    }

    #[test]
    fn a_generated_password_is_very_strong() {
        let s = estimate("Kx9#mP2$vL8@qR4!nW6z", &[]);
        assert_eq!(s.score, 4);
        assert_eq!(s.label, "Very strong");
    }

    #[test]
    fn user_context_lowers_the_score_of_a_password_built_from_it() {
        // A password that reuses something the user is known by — their
        // workspace name here — is guessable by anyone who knows them. zxcvbn
        // treats an exact match to a supplied context term as instantly
        // guessable, so passing that context must both lower the score and never
        // raise it. This is why the API takes the user's email and workspace.
        let term = "AcmeWorkspace";
        let bare = estimate(term, &[]);
        let with_context = estimate(term, &[term]);
        assert!(
            with_context.score < bare.score,
            "context should lower a context-derived password (bare {}, ctx {})",
            bare.score,
            with_context.score
        );
        assert!(!with_context.acceptable);
    }

    #[test]
    fn feedback_is_specific_when_available() {
        // A weak password should come with an actionable suggestion, not silence.
        let s = estimate("password1", &[]);
        assert!(s.score <= 1);
        assert!(
            s.suggestion.is_some() || s.warning.is_some(),
            "a weak password should carry guidance"
        );
    }

    #[test]
    fn the_stakes_are_reported_as_an_order_of_magnitude() {
        let weak = estimate("123456", &[]);
        let strong = estimate("Kx9#mP2$vL8@qR4!nW6z", &[]);
        assert!(weak.guesses_log10.starts_with("10^"));
        assert!(strong.guesses_log10.starts_with("10^"));
        assert_ne!(weak.guesses_log10, strong.guesses_log10);
    }
}
