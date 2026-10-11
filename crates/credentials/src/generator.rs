//! Password and passphrase generation.
//!
//! All randomness comes from `OsRng`, and character selection uses rejection
//! sampling via `gen_range` rather than modulo arithmetic, so every allowed
//! character is equally likely.

use anyhow::{Result, bail};
use rand::Rng;
use rand::seq::SliceRandom;

const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &[u8] = b"0123456789";
const SYMBOLS: &[u8] = b"!@#$%^&*()-_=+[]{};:,.?";
/// Excluded under `avoid_ambiguous`: glyphs that are easy to misread aloud or in
/// a monospace font.
const AMBIGUOUS: &[u8] = b"O0oIl1|`'\"";

/// Which character classes to draw from, and how long the result should be.
#[derive(Debug, Clone)]
pub struct PasswordSpec {
    pub length: usize,
    pub lowercase: bool,
    pub uppercase: bool,
    pub digits: bool,
    pub symbols: bool,
    pub avoid_ambiguous: bool,
}

impl Default for PasswordSpec {
    fn default() -> Self {
        Self {
            length: 20,
            lowercase: true,
            uppercase: true,
            digits: true,
            symbols: true,
            avoid_ambiguous: false,
        }
    }
}

/// Generate a password matching `spec`.
///
/// Guarantees at least one character from each enabled class, which is why the
/// minimum length is the number of enabled classes.
pub fn generate_password(spec: &PasswordSpec) -> Result<String> {
    let mut classes: Vec<Vec<u8>> = Vec::new();
    for (enabled, set) in [
        (spec.lowercase, LOWER),
        (spec.uppercase, UPPER),
        (spec.digits, DIGITS),
        (spec.symbols, SYMBOLS),
    ] {
        if !enabled {
            continue;
        }
        let filtered: Vec<u8> = set
            .iter()
            .copied()
            .filter(|c| !spec.avoid_ambiguous || !AMBIGUOUS.contains(c))
            .collect();
        if !filtered.is_empty() {
            classes.push(filtered);
        }
    }

    if classes.is_empty() {
        bail!("at least one character class must be enabled");
    }
    if spec.length < classes.len() {
        bail!(
            "length {} is too short to include all {} selected character classes",
            spec.length,
            classes.len()
        );
    }

    let mut rng = rand::rngs::OsRng;
    let mut out: Vec<u8> = Vec::with_capacity(spec.length);

    // One from each class first, so the result always satisfies the spec.
    for class in &classes {
        out.push(class[rng.gen_range(0..class.len())]);
    }

    let pool: Vec<u8> = classes.concat();
    while out.len() < spec.length {
        out.push(pool[rng.gen_range(0..pool.len())]);
    }

    // Without this the first characters would be class-ordered and predictable.
    out.shuffle(&mut rng);
    Ok(String::from_utf8(out).expect("all character sets are ASCII"))
}

/// A short, deliberately dependency-free word list for passphrases.
///
/// 256 words gives 8 bits per word: a 5-word passphrase carries 40 bits. That is
/// weaker than EFF's 7776-word list (12.9 bits/word) — prefer
/// [`generate_password`] for anything protecting a vault, and treat passphrases
/// as a convenience for secrets a human must read aloud or retype.
const WORDS: &[&str] = &[
    "amber", "anchor", "apple", "arbor", "arrow", "aspen", "atlas", "autumn", "bacon", "badge",
    "bamboo", "banjo", "barley", "basin", "beacon", "beetle", "birch", "bishop", "bison",
    "blossom", "bolt", "bonsai", "border", "bottle", "boulder", "branch", "brass", "bridge",
    "bronze", "brook", "bucket", "buffalo", "bundle", "butter", "cabin", "cactus", "camera",
    "candle", "canoe", "canyon", "carbon", "cargo", "carrot", "castle", "cedar", "cellar",
    "cement", "cherry", "chimney", "cinder", "circle", "citrus", "clover", "cobalt", "cocoa",
    "comet", "compass", "copper", "coral", "cotton", "cougar", "crater", "crayon", "creek",
    "crimson", "crystal", "cypress", "dagger", "dahlia", "daisy", "dapple", "dawn", "delta",
    "denim", "desert", "diamond", "dolphin", "domino", "donkey", "dragon", "dune", "eagle",
    "ember", "emerald", "engine", "ermine", "falcon", "fathom", "feather", "fennel", "fern",
    "fiddle", "filter", "flint", "forest", "fossil", "fountain", "fox", "gadget", "galaxy",
    "garden", "garnet", "gecko", "geyser", "ginger", "glacier", "granite", "gravel", "grotto",
    "guitar", "gully", "hammer", "harbor", "harvest", "hazel", "heather", "hedge", "helium",
    "hollow", "honey", "hornet", "hunter", "indigo", "iris", "ivory", "jacket", "jasmine",
    "jasper", "jetty", "jigsaw", "jungle", "juniper", "kayak", "kernel", "kettle", "keystone",
    "lagoon", "lancer", "lantern", "lattice", "laurel", "lemon", "lichen", "lilac", "linen",
    "lobster", "locket", "lotus", "lumber", "lunar", "magnet", "mahogany", "mallet", "mango",
    "maple", "marble", "marina", "meadow", "melon", "meteor", "mineral", "mint", "mirror",
    "monsoon", "mosaic", "moss", "mulberry", "nectar", "needle", "nickel", "nimbus", "noble",
    "nomad", "nutmeg", "oasis", "obsidian", "ocean", "onyx", "opal", "orchard", "orchid", "osprey",
    "otter", "oxide", "paddle", "palace", "pampas", "papaya", "parcel", "pasture", "pebble",
    "pelican", "pepper", "petal", "pewter", "pigeon", "pillar", "pine", "piston", "plateau",
    "plum", "pollen", "poppy", "portal", "prairie", "prism", "pueblo", "quarry", "quartz",
    "quiver", "radish", "rafter", "rapid", "raven", "reef", "relay", "rhubarb", "ribbon", "ridge",
    "rimrock", "river", "rocket", "rosemary", "rubble", "ruby", "rudder", "saffron", "sage",
    "salmon", "sandbar", "sapphire", "satchel", "scarlet", "sequoia", "shadow", "shale", "shelter",
    "sierra", "signal", "silver", "sitka", "slate", "sleet", "socket", "sorrel", "spruce",
    "stellar", "stone", "summit", "sunset", "syrup", "tabby", "talon", "tamarind", "tandem",
    "tapestry", "teak", "tempo", "thicket", "thistle", "thunder", "timber", "topaz", "torrent",
    "tulip", "tundra", "turbine", "umber", "vanilla", "velvet", "vessel", "violet", "walnut",
    "willow",
];

/// Generate a `words`-word passphrase joined by `separator`.
pub fn generate_passphrase(words: usize, separator: &str, capitalize: bool) -> Result<String> {
    if words == 0 {
        bail!("passphrase must contain at least one word");
    }
    let mut rng = rand::rngs::OsRng;
    let parts: Vec<String> = (0..words)
        .map(|_| {
            let w = WORDS[rng.gen_range(0..WORDS.len())];
            if capitalize {
                let mut c = w.chars();
                match c.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            } else {
                w.to_string()
            }
        })
        .collect();
    Ok(parts.join(separator))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_length() {
        let spec = PasswordSpec {
            length: 32,
            ..Default::default()
        };
        assert_eq!(generate_password(&spec).unwrap().chars().count(), 32);
    }

    #[test]
    fn includes_every_enabled_class() {
        let spec = PasswordSpec {
            length: 12,
            ..Default::default()
        };
        // Probabilistic guarantees are not enough; the generator seeds one of
        // each class explicitly, so this must hold on every run.
        for _ in 0..200 {
            let p = generate_password(&spec).unwrap();
            assert!(p.chars().any(|c| c.is_ascii_lowercase()), "{p}");
            assert!(p.chars().any(|c| c.is_ascii_uppercase()), "{p}");
            assert!(p.chars().any(|c| c.is_ascii_digit()), "{p}");
            assert!(
                p.chars().any(|c| SYMBOLS.contains(&(c as u8))),
                "{p} has no symbol"
            );
        }
    }

    #[test]
    fn honours_disabled_classes() {
        let spec = PasswordSpec {
            length: 24,
            symbols: false,
            digits: false,
            ..Default::default()
        };
        let p = generate_password(&spec).unwrap();
        assert!(p.chars().all(|c| c.is_ascii_alphabetic()), "{p}");
    }

    #[test]
    fn avoids_ambiguous_characters_when_asked() {
        let spec = PasswordSpec {
            length: 40,
            avoid_ambiguous: true,
            ..Default::default()
        };
        for _ in 0..100 {
            let p = generate_password(&spec).unwrap();
            assert!(
                !p.chars().any(|c| AMBIGUOUS.contains(&(c as u8))),
                "{p} contains an ambiguous character"
            );
        }
    }

    #[test]
    fn rejects_impossible_specs() {
        assert!(
            generate_password(&PasswordSpec {
                length: 2,
                ..Default::default()
            })
            .is_err(),
            "length below the number of enabled classes must fail"
        );
        assert!(
            generate_password(&PasswordSpec {
                lowercase: false,
                uppercase: false,
                digits: false,
                symbols: false,
                ..Default::default()
            })
            .is_err(),
            "no enabled classes must fail"
        );
    }

    #[test]
    fn passwords_do_not_repeat() {
        let spec = PasswordSpec::default();
        let a = generate_password(&spec).unwrap();
        let b = generate_password(&spec).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn passphrase_shape() {
        let p = generate_passphrase(5, "-", true).unwrap();
        assert_eq!(p.split('-').count(), 5);
        assert!(p.chars().next().unwrap().is_ascii_uppercase());
        assert!(generate_passphrase(0, "-", false).is_err());
    }
}
