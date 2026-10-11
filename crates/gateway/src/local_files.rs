//! The narrow, jailed door between the assistant and your actual disk.
//!
//! Everything else the chat agent can do writes to the database. These three
//! operations — a note on your Desktop, a folder, a file inside it — are the
//! only ones that touch the filesystem, and they are deliberately the smallest
//! surface that satisfies "put a note on my desktop" and "make me a folder".
//!
//! # The boundary
//!
//! Two roots, nothing else:
//!
//! * **notes** (`~/Desktop` by default) — where a note lands so you actually see
//!   it. Notes only: a fixed `.md` extension, a sanitised filename, no
//!   subdirectories. The Desktop is not a place to let a language model build a
//!   directory tree.
//! * **projects** (`~/Consistent` by default) — folders and files, nested freely.
//!
//! A path outside both is refused. There is no "allow everything" setting,
//! because the failure mode of one is a model that read a hostile web page and
//! was talked into writing to `~/.ssh/authorized_keys`.
//!
//! # Why the jail is not just a string prefix check
//!
//! `root.join(relative)` followed by `starts_with(root)` is the obvious
//! implementation and it is wrong twice over. `..` walks out before the check
//! ever runs, and a symlink placed inside the root points anywhere at all while
//! still *looking* like a child. So [`jail`] normalises `.`/`..` lexically and
//! then canonicalises the deepest part of the path that actually exists,
//! re-checking that against the canonical root — which is what makes a symlink
//! escape fail. Both are pinned by tests.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// Cap on a generated note filename, before `.md`. Long enough for a real
/// title, short enough to survive every filesystem we care about.
const MAX_NOTE_STEM: usize = 60;
/// Refuse to write a note larger than this. A runaway model should not be able
/// to fill a disk one note at a time.
const MAX_NOTE_BYTES: usize = 1024 * 1024;

/// Where the assistant may write, resolved once at startup.
#[derive(Debug, Clone)]
pub struct LocalFiles {
    /// Notes land here, flat. Typically `~/Desktop`.
    pub notes_root: PathBuf,
    /// Folders and files live here. Typically `~/Consistent`.
    pub projects_root: PathBuf,
}

impl LocalFiles {
    pub fn new(notes_root: impl Into<PathBuf>, projects_root: impl Into<PathBuf>) -> Self {
        Self {
            notes_root: notes_root.into(),
            projects_root: projects_root.into(),
        }
    }

    /// The defaults: notes on the Desktop, work under `~/Consistent`.
    pub fn from_home(home: &Path) -> Self {
        Self::new(home.join("Desktop"), home.join("Consistent"))
    }

    /// Create both roots if they do not exist yet, then pin them to their
    /// canonical form.
    ///
    /// `~/Desktop` exists on every desktop OS, but `~/Consistent` will not on a
    /// first run, and a tool that fails because its own root is missing is a
    /// bad first impression.
    ///
    /// Canonicalising here matters beyond tidiness: on macOS `/var` is a
    /// symlink to `/private/var`, so a root and the paths [`jail`] returns
    /// under it would otherwise disagree about their own prefix — and every
    /// "is this inside the root" comparison a caller makes would be wrong.
    pub fn ensure_roots(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.notes_root)?;
        std::fs::create_dir_all(&self.projects_root)?;
        if let Ok(c) = self.notes_root.canonicalize() {
            self.notes_root = c;
        }
        if let Ok(c) = self.projects_root.canonicalize() {
            self.projects_root = c;
        }
        Ok(())
    }

    /// Write a note where the user will see it. Returns the path written.
    ///
    /// Same-titled notes do not overwrite each other — the second becomes
    /// `title-2.md`. Silently replacing something you wrote yesterday because
    /// today's note has the same title is not a behaviour anyone wants.
    pub fn save_note(&self, title: Option<&str>, content: &str) -> Result<PathBuf> {
        let body = content.trim();
        if body.is_empty() {
            bail!("refusing to write an empty note");
        }
        if body.len() > MAX_NOTE_BYTES {
            bail!("note is too large ({} bytes)", body.len());
        }
        std::fs::create_dir_all(&self.notes_root)?;

        let stem = note_stem(title, body);
        let path = self.unique_note_path(&stem);
        // The filename is generated, never taken from the model verbatim, but
        // run it through the jail anyway: one refactor away from being input.
        let checked = jail(
            &self.notes_root,
            path.file_name().unwrap().to_str().unwrap(),
        )?;
        std::fs::write(&checked, note_document(title, body))?;
        Ok(checked)
    }

    /// `mkdir -p` inside the projects root. Returns the folder created.
    pub fn create_folder(&self, relative: &str) -> Result<PathBuf> {
        let target = jail(&self.projects_root, relative)?;
        std::fs::create_dir_all(&target)?;
        Ok(target)
    }

    /// Write a file inside the projects root, creating parent folders.
    pub fn write_file(&self, relative: &str, content: &str) -> Result<PathBuf> {
        if content.len() > MAX_NOTE_BYTES {
            bail!("file is too large ({} bytes)", content.len());
        }
        let target = jail(&self.projects_root, relative)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        Ok(target)
    }

    /// First free `stem.md`, `stem-2.md`, ... Bounded so a pathological case
    /// gives up rather than spinning.
    fn unique_note_path(&self, stem: &str) -> PathBuf {
        let first = self.notes_root.join(format!("{stem}.md"));
        if !first.exists() {
            return first;
        }
        for n in 2..1000 {
            let candidate = self.notes_root.join(format!("{stem}-{n}.md"));
            if !candidate.exists() {
                return candidate;
            }
        }
        self.notes_root
            .join(format!("{stem}-{}.md", uuid::Uuid::new_v4().simple()))
    }
}

/// Resolve `relative` under `root`, or refuse.
///
/// Refuses absolute paths, anything that climbs out with `..`, and anything
/// whose existing ancestry canonicalises outside the root — which is how a
/// symlink escape is caught. The returned path is *not* guaranteed to exist;
/// it is guaranteed to be inside the root.
pub fn jail(root: &Path, relative: &str) -> Result<PathBuf> {
    let raw = relative.trim();
    if raw.is_empty() {
        bail!("no path given");
    }
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        bail!("absolute paths are not allowed; give a path relative to the folder");
    }
    if raw.contains('\0') {
        bail!("path contains a null byte");
    }

    let canonical_root = root
        .canonicalize()
        .unwrap_or_else(|_| normalize_lexically(root));

    // Lexical pass: apply `..` against our own accumulator so it can never
    // climb above the root, whatever the string looks like.
    let mut resolved = canonical_root.clone();
    for comp in candidate.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved == canonical_root {
                    bail!("path escapes the allowed folder");
                }
                resolved.pop();
                if !resolved.starts_with(&canonical_root) {
                    bail!("path escapes the allowed folder");
                }
            }
            Component::Normal(part) => resolved.push(part),
            // A second root or prefix inside a "relative" path is nonsense.
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths are not allowed")
            }
        }
    }
    if !resolved.starts_with(&canonical_root) {
        bail!("path escapes the allowed folder");
    }

    // Symlink pass: whatever part of this path already exists must really live
    // under the root once links are followed.
    let mut existing = resolved.as_path();
    loop {
        match existing.canonicalize() {
            Ok(real) => {
                if !real.starts_with(&canonical_root) {
                    bail!("path escapes the allowed folder through a link");
                }
                break;
            }
            // Not created yet — check its parent instead.
            Err(_) => match existing.parent() {
                Some(parent) if parent.starts_with(&canonical_root) => existing = parent,
                _ => break,
            },
        }
    }

    Ok(resolved)
}

/// Best-effort `.`/`..` collapse for a path that does not exist yet.
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A filename from a title (or the note's first line), safe on every platform.
fn note_stem(title: Option<&str>, body: &str) -> String {
    let source = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| body.lines().next().unwrap_or("note").trim());

    let mut stem = String::with_capacity(MAX_NOTE_STEM);
    let mut last_dash = false;
    for ch in source.chars() {
        // Deliberately strict: only alphanumerics survive as themselves.
        // Everything else — separators, dots, control characters, emoji —
        // collapses to a single dash, so `a/b/c` reads as `a-b-c` rather than
        // silently becoming `abc`, and no path separator can ever reach the
        // filesystem.
        if ch.is_alphanumeric() {
            stem.push(ch);
            last_dash = false;
        } else if !last_dash && !stem.is_empty() {
            stem.push('-');
            last_dash = true;
        }
        if stem.chars().count() >= MAX_NOTE_STEM {
            break;
        }
    }
    let trimmed = stem.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "note".to_string()
    } else {
        trimmed
    }
}

/// The note as it lands on disk: a title line, a timestamp, then the text.
/// Markdown because the file is for a human to open, not for us to re-parse.
fn note_document(title: Option<&str>, body: &str) -> String {
    let heading = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Note");
    format!(
        "# {heading}\n\n_Saved by Consistent on {}_\n\n{body}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (tempfile::TempDir, LocalFiles) {
        let dir = tempfile::tempdir().unwrap();
        let mut files = LocalFiles::new(dir.path().join("Desktop"), dir.path().join("Consistent"));
        files.ensure_roots().unwrap();
        (dir, files)
    }

    #[test]
    fn a_note_lands_on_the_desktop_as_readable_markdown() {
        let (_dir, files) = roots();
        let path = files
            .save_note(Some("Launch checklist"), "ship the DMG")
            .unwrap();

        assert_eq!(path.parent().unwrap(), files.notes_root);
        assert_eq!(path.file_name().unwrap(), "Launch-checklist.md");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Launch checklist"));
        assert!(text.contains("ship the DMG"));
    }

    #[test]
    fn a_second_note_with_the_same_title_does_not_overwrite_the_first() {
        let (_dir, files) = roots();
        let a = files.save_note(Some("Ideas"), "first").unwrap();
        let b = files.save_note(Some("Ideas"), "second").unwrap();
        assert_ne!(a, b);
        assert!(std::fs::read_to_string(&a).unwrap().contains("first"));
        assert!(std::fs::read_to_string(&b).unwrap().contains("second"));
    }

    #[test]
    fn a_title_cannot_smuggle_a_path_into_the_filename() {
        let (_dir, files) = roots();
        // Every one of these must stay a flat file on the Desktop.
        for hostile in [
            "../../etc/passwd",
            "/etc/passwd",
            "..\\..\\windows\\system32",
            "note/../../escape",
        ] {
            let path = files.save_note(Some(hostile), "body").unwrap();
            assert_eq!(
                path.parent().unwrap(),
                files.notes_root,
                "{hostile} escaped the notes folder"
            );
        }
    }

    #[test]
    fn an_empty_or_oversized_note_is_refused() {
        let (_dir, files) = roots();
        assert!(files.save_note(None, "   ").is_err());
        assert!(
            files
                .save_note(None, &"x".repeat(MAX_NOTE_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn folders_and_files_are_created_under_the_projects_root() {
        let (_dir, files) = roots();
        let folder = files.create_folder("clients/acme/invoices").unwrap();
        assert!(folder.is_dir());
        assert!(folder.starts_with(&files.projects_root));

        let file = files.write_file("clients/acme/notes.md", "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
    }

    /// The whole point of the jail.
    #[test]
    fn paths_that_climb_out_are_refused() {
        let (_dir, files) = roots();
        for hostile in [
            "../escaped",
            "../../escaped",
            "clients/../../escaped",
            "/etc/passwd",
            "/tmp/anywhere",
            "..",
        ] {
            assert!(
                files.create_folder(hostile).is_err(),
                "{hostile} should have been refused"
            );
            assert!(
                files.write_file(hostile, "x").is_err(),
                "{hostile} should have been refused for write"
            );
        }
    }

    #[test]
    fn a_path_that_stays_inside_after_climbing_is_allowed() {
        let (_dir, files) = roots();
        // Not an escape: down one, back up, down again — still inside.
        let created = files.create_folder("a/../b").unwrap();
        assert_eq!(created, files.projects_root.join("b"));
    }

    /// A symlink inside the root still *looks* like a child to a prefix check.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_be_used_to_escape() {
        let (dir, files) = roots();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, files.projects_root.join("link")).unwrap();

        assert!(
            files.write_file("link/pwned.txt", "x").is_err(),
            "writing through a symlink escaped the jail"
        );
        assert!(!outside.join("pwned.txt").exists());
    }

    #[test]
    fn note_stems_are_sane() {
        assert_eq!(note_stem(Some("Launch checklist"), ""), "Launch-checklist");
        assert_eq!(note_stem(Some("  "), "first line here"), "first-line-here");
        assert_eq!(note_stem(Some("!!!"), "!!!"), "note");
        assert_eq!(note_stem(Some("a/b/c"), ""), "a-b-c");
        assert!(note_stem(Some(&"x".repeat(500)), "").chars().count() <= MAX_NOTE_STEM);
    }

    #[test]
    fn the_jail_rejects_absolute_and_empty_paths() {
        let (_dir, files) = roots();
        assert!(jail(&files.projects_root, "").is_err());
        assert!(jail(&files.projects_root, "   ").is_err());
        assert!(jail(&files.projects_root, "/absolute").is_err());
    }
}
