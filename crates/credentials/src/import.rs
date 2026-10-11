//! Importing from other password managers.
//!
//! The single biggest thing standing between someone who downloads this and
//! someone who *uses* it is the passwords they already have somewhere else. So
//! this reads the CSV exports that Bitwarden, Chrome/Edge/Brave, 1Password,
//! LastPass and Safari produce, plus anything close enough to a generic
//! `name,url,username,password` shape, and turns them into [`NewItem`]s the vault
//! can store.
//!
//! # Design choices worth knowing
//!
//! - **The CSV reader is hand-written** (RFC 4180). A password import runs over a
//!   file full of the user's real credentials, so the parser is small enough to
//!   audit in one sitting rather than a dependency with its own surface. It
//!   handles the parts that actually break naive splitters: quoted fields,
//!   embedded commas, embedded newlines, and escaped quotes.
//! - **Columns are matched by meaning, not position.** Each known exporter has a
//!   header this recognises, but the fallback matches on what a column *is*
//!   (anything containing "user" is a username), so an unlisted manager usually
//!   imports anyway.
//! - **TOTP seeds come across.** A migration that dropped the 2FA seeds would
//!   send the user back to their old app for exactly the accounts they most want
//!   moved.
//! - **Nothing is decrypted or validated here.** This turns bytes into items; the
//!   caller stores them through the normal vault path, so imported items get
//!   fresh per-item keys like any other.

use anyhow::{Result, bail};

use crate::model::{CustomField, ItemKind, ItemSecret, NewItem};

/// Which exporter a file came from, detected from its header row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Bitwarden,
    Chrome,
    OnePassword,
    LastPass,
    Safari,
    /// Recognisable as a login CSV, but not a specific known exporter.
    Generic,
}

impl ImportFormat {
    /// A human-readable name for showing which exporter was detected.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bitwarden => "Bitwarden",
            Self::Chrome => "Chrome / Edge / Brave",
            Self::OnePassword => "1Password",
            Self::LastPass => "LastPass",
            Self::Safari => "Safari / iCloud Keychain",
            Self::Generic => "Generic CSV",
        }
    }
}

/// The result of a parse: what was recognised, and what could not be.
#[derive(Debug, Clone)]
pub struct ImportResult {
    /// The exporter [`detect_format`] recognised from the header row.
    pub format: ImportFormat,
    /// Rows that parsed into a storable login.
    pub items: Vec<NewItem>,
    /// One message per row that was skipped, so the user learns *which* rows did
    /// not come across rather than just a count. A silent partial import is how
    /// someone loses one password and finds out months later.
    pub skipped: Vec<String>,
}

/// Parse a CSV export into importable items, detecting the source format.
pub fn parse_csv(contents: &str) -> Result<ImportResult> {
    let rows = read_csv(contents)?;
    let mut rows = rows.into_iter();

    let header = rows.next().ok_or_else(anyhow_empty)?;
    let header_lower: Vec<String> = header.iter().map(|h| h.trim().to_lowercase()).collect();

    let format = detect_format(&header_lower);
    let mapping = ColumnMapping::from_header(&header_lower);

    if mapping.password.is_none() && mapping.username.is_none() && mapping.totp.is_none() {
        bail!(
            "this does not look like a password export — no username, password or TOTP column found"
        );
    }

    let mut items = Vec::new();
    let mut skipped = Vec::new();

    for (i, row) in rows.enumerate() {
        // +2: 1 for the header we consumed, 1 to count from line 1.
        let line = i + 2;
        if row.iter().all(|c| c.trim().is_empty()) {
            continue; // trailing blank line, common in exports
        }
        match mapping.to_item(&row) {
            Ok(Some(item)) => items.push(item),
            Ok(None) => skipped.push(format!("row {line}: nothing to import (empty login)")),
            Err(e) => skipped.push(format!("row {line}: {e}")),
        }
    }

    if items.is_empty() {
        bail!("no importable logins were found in this file");
    }

    Ok(ImportResult {
        format,
        items,
        skipped,
    })
}

fn anyhow_empty() -> anyhow::Error {
    anyhow::anyhow!("the file is empty")
}

// Each exporter is identified by a distinctive header column or combination,
// checked most-specific first so a generic CSV never gets misattributed.
fn detect_format(header: &[String]) -> ImportFormat {
    let has = |name: &str| header.iter().any(|h| h == name);

    if has("login_password") || has("login_uri") {
        ImportFormat::Bitwarden
    } else if has("otpauth") && has("title") {
        ImportFormat::Safari
    } else if has("grouping") && has("extra") {
        ImportFormat::LastPass
    } else if has("title") && (has("otpauth") || has("one-time password")) {
        ImportFormat::OnePassword
    } else if header == ["name", "url", "username", "password"]
        || header == ["name", "url", "username", "password", "note"]
    {
        ImportFormat::Chrome
    } else {
        ImportFormat::Generic
    }
}

/// Which column holds which field, resolved once from the header.
struct ColumnMapping {
    name: Option<usize>,
    username: Option<usize>,
    password: Option<usize>,
    url: Option<usize>,
    totp: Option<usize>,
    notes: Option<usize>,
    folder: Option<usize>,
}

impl ColumnMapping {
    /// Match columns by what they mean. Exporters disagree on names — url vs
    /// website vs login_uri, notes vs extra vs comment — so this looks for the
    /// concept, most-specific spelling first.
    fn from_header(header: &[String]) -> Self {
        let find = |candidates: &[&str]| -> Option<usize> {
            // Exact match wins over a substring match, so "username" is not
            // shadowed by a stray "user id" column.
            for cand in candidates {
                if let Some(i) = header.iter().position(|h| h == cand) {
                    return Some(i);
                }
            }
            for cand in candidates {
                if let Some(i) = header.iter().position(|h| h.contains(cand)) {
                    return Some(i);
                }
            }
            None
        };

        Self {
            name: find(&["name", "title", "account"]),
            username: find(&["login_username", "username", "user", "login", "email"]),
            password: find(&["login_password", "password", "pass"]),
            url: find(&["login_uri", "url", "website", "uri", "site"]),
            totp: find(&["login_totp", "otpauth", "one-time password", "totp", "otp"]),
            notes: find(&["notes", "note", "extra", "comment"]),
            folder: find(&["folder", "grouping", "category", "group"]),
        }
    }

    // Read one mapped column, treating a blank cell the same as a missing one.
    fn cell(row: &[String], index: Option<usize>) -> Option<String> {
        let value = index.and_then(|i| row.get(i))?.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }

    /// Build one item from a row, or `None` if there is nothing worth storing.
    fn to_item(&self, row: &[String]) -> Result<Option<NewItem>> {
        let username = Self::cell(row, self.username);
        let password = Self::cell(row, self.password);
        let totp = Self::cell(row, self.totp);
        let notes = Self::cell(row, self.notes);

        // A row with no secret material at all is not worth an item.
        if username.is_none() && password.is_none() && totp.is_none() && notes.is_none() {
            return Ok(None);
        }

        let url = Self::cell(row, self.url);
        let name = Self::cell(row, self.name)
            .or_else(|| url.clone())
            .or_else(|| username.clone())
            .unwrap_or_else(|| "Imported login".to_string());

        // A TOTP-shaped otpauth URI is a login secret, not a note.
        let totp_secret = totp.filter(|t| !t.is_empty());

        let uris = url.into_iter().collect();

        Ok(Some(NewItem {
            kind: ItemKind::Login,
            name,
            // Parsed items land in the global set; the caller refiles them into
            // an entity via `ImportResult::into_scope` before storing.
            scope: crate::scope::Scope::Global,
            folder: Self::cell(row, self.folder),
            uris,
            favorite: false,
            secret: ItemSecret {
                username,
                password,
                totp_secret,
                notes,
                custom_fields: Vec::<CustomField>::new(),
                password_history: Vec::new(),
            },
        }))
    }
}

impl ImportResult {
    /// File every parsed item under one entity. Called by the import endpoint so
    /// a user can import a CSV straight into a specific entity's password set.
    pub fn into_scope(mut self, scope: crate::scope::Scope) -> Self {
        for item in &mut self.items {
            item.scope = scope.clone();
        }
        self
    }
}

/// A minimal RFC 4180 CSV reader.
///
/// Handles quoted fields, embedded commas and newlines, and doubled quotes as an
/// escape. Deliberately not a general CSV library — it does what password exports
/// need and nothing more, which keeps it small enough to trust with a file full
/// of real credentials.
fn read_csv(input: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();
    // Track whether the current field/row has seen any content, so we can tell a
    // genuine empty trailing line from a real empty final field.
    let mut any_content_in_row = false;

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                other => field.push(other),
            }
            continue;
        }

        match c {
            '"' => {
                in_quotes = true;
                any_content_in_row = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                any_content_in_row = true;
            }
            '\r' => { /* swallow; the \n handles the line break */ }
            '\n' => {
                row.push(std::mem::take(&mut field));
                if any_content_in_row {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
                any_content_in_row = false;
            }
            other => {
                field.push(other);
                any_content_in_row = true;
            }
        }
    }

    if in_quotes {
        bail!("the file has an unterminated quoted field and may be corrupt");
    }

    // Flush a final row with no trailing newline.
    if any_content_in_row || !field.is_empty() {
        row.push(field);
        rows.push(row);
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quoted_fields_with_commas_and_newlines() {
        let csv = "a,b,c\n\"has, comma\",\"has\nnewline\",\"has \"\"quotes\"\"\"";
        let rows = read_csv(csv).unwrap();
        assert_eq!(
            rows[1],
            vec!["has, comma", "has\nnewline", "has \"quotes\""]
        );
    }

    #[test]
    fn trailing_blank_lines_are_ignored() {
        let rows = read_csv("a,b\n1,2\n\n\n").unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn imports_a_bitwarden_export() {
        // The columns Bitwarden actually writes.
        let csv = "folder,favorite,type,name,notes,fields,reprompt,login_uri,login_username,login_password,login_totp\n\
                   Work,1,login,GitHub,my note,,0,https://github.com,ahmad,hunter2,GEZDGNBVGY3TQOJQ";
        let result = parse_csv(csv).unwrap();
        assert_eq!(result.format, ImportFormat::Bitwarden);
        assert_eq!(result.items.len(), 1);

        let item = &result.items[0];
        assert_eq!(item.name, "GitHub");
        assert_eq!(item.folder.as_deref(), Some("Work"));
        assert_eq!(item.uris, vec!["https://github.com"]);
        assert_eq!(item.secret.username.as_deref(), Some("ahmad"));
        assert_eq!(item.secret.password.as_deref(), Some("hunter2"));
        assert_eq!(item.secret.totp_secret.as_deref(), Some("GEZDGNBVGY3TQOJQ"));
        assert_eq!(item.secret.notes.as_deref(), Some("my note"));
    }

    #[test]
    fn imports_a_chrome_export() {
        let csv = "name,url,username,password\n\
                   GitHub,https://github.com,ahmad,hunter2\n\
                   Gmail,https://mail.google.com,ahmad@gmail.com,s3cret";
        let result = parse_csv(csv).unwrap();
        assert_eq!(result.format, ImportFormat::Chrome);
        assert_eq!(result.items.len(), 2);
        assert_eq!(
            result.items[1].secret.username.as_deref(),
            Some("ahmad@gmail.com")
        );
    }

    #[test]
    fn imports_a_lastpass_export() {
        let csv = "url,username,password,totp,extra,name,grouping,fav\n\
                   https://github.com,ahmad,hunter2,GEZDGNBVGY3TQOJQ,some note,GitHub,Work,0";
        let result = parse_csv(csv).unwrap();
        assert_eq!(result.format, ImportFormat::LastPass);
        let item = &result.items[0];
        assert_eq!(item.name, "GitHub");
        assert_eq!(item.secret.totp_secret.as_deref(), Some("GEZDGNBVGY3TQOJQ"));
        assert_eq!(item.folder.as_deref(), Some("Work"));
    }

    #[test]
    fn imports_a_generic_csv_by_column_meaning() {
        // Unknown exporter, spaced-out column names — still recognisable because
        // matching is on meaning: "Web Site" contains site, "User Name" contains
        // user, "Pass Phrase" contains pass.
        let csv = "Web Site,User Name,Pass Phrase\n\
                   example.com,me@example.com,pw123";
        let result = parse_csv(csv).unwrap();
        assert_eq!(result.format, ImportFormat::Generic);
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].secret.username.as_deref(),
            Some("me@example.com")
        );
        assert_eq!(result.items[0].secret.password.as_deref(), Some("pw123"));
        assert_eq!(result.items[0].uris, vec!["example.com"]);
    }

    #[test]
    fn a_row_missing_its_name_falls_back_to_url_then_username() {
        let csv = "name,url,username,password\n\
                   ,https://only-url.com,,pw\n\
                   ,,only-user,pw";
        let result = parse_csv(csv).unwrap();
        assert_eq!(result.items[0].name, "https://only-url.com");
        assert_eq!(result.items[1].name, "only-user");
    }

    #[test]
    fn empty_rows_are_skipped_not_imported_as_blanks() {
        let csv = "name,url,username,password\n\
                   Real,https://x.com,u,p\n\
                   ,,,";
        let result = parse_csv(csv).unwrap();
        assert_eq!(
            result.items.len(),
            1,
            "the empty row must not become an item"
        );
    }

    #[test]
    fn skipped_rows_are_reported_by_line_number() {
        // A row with a name but genuinely no secret material is reported, not
        // silently dropped.
        let csv = "name,url,username,password,notes\n\
                   Good,https://x.com,u,p,\n\
                   Bare,,,,";
        let result = parse_csv(csv).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.skipped.len(), 1);
        assert!(result.skipped[0].contains("row 3"), "{:?}", result.skipped);
    }

    #[test]
    fn a_file_that_is_not_a_password_export_is_refused() {
        let csv = "date,amount,description\n2026-01-01,42.00,coffee";
        assert!(parse_csv(csv).is_err());
    }

    #[test]
    fn an_empty_file_is_refused() {
        assert!(parse_csv("").is_err());
    }

    #[test]
    fn an_unterminated_quote_is_an_error_not_a_panic() {
        assert!(read_csv("a,b\n\"unclosed,c").is_err());
    }
}
