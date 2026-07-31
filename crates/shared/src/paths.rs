//! Syntactic rules for vault-relative paths.
//!
//! These live in the shared crate so the server and the frontend enforce
//! byte-for-byte identical rules — the UI can grey out an illegal folder name
//! using exactly the check the server will later apply.
//!
//! This module deliberately does **not** touch the filesystem. It is the first
//! of two gates; the server's `VaultPath` adds canonicalisation and symlink
//! containment on top. Passing these checks means a path is *well-formed*, not
//! that it is safe to open.

use std::fmt;

/// Longest single path component, in bytes. Most filesystems allow 255; we stay
/// under it so the server can append suffixes (`-1`, `.tmp`) without overflowing.
pub const MAX_COMPONENT_BYTES: usize = 200;

/// Longest full vault-relative path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;

/// Directory holding soft-deleted notes. Users can never address it directly:
/// it begins with a dot, which the component rules reject.
pub const TRASH_DIR: &str = ".trash";

/// Directory that uploads are written to.
pub const ATTACHMENTS_DIR: &str = "attachments";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    TooLong,
    Absolute,
    Backslash,
    EmptyComponent,
    DotComponent,
    ParentComponent,
    ControlCharacter,
    IllegalCharacter(char),
    LeadingDot,
    LeadingOrTrailingSpace,
    TrailingDot,
    ComponentTooLong,
    ReservedName,
    NotMarkdown,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            PathError::Empty => "path is empty".into(),
            PathError::TooLong => "path is too long".into(),
            PathError::Absolute => "path must be relative to the vault".into(),
            PathError::Backslash => "backslashes are not allowed; use '/'".into(),
            PathError::EmptyComponent => "path contains an empty segment".into(),
            PathError::DotComponent => "'.' is not a valid name".into(),
            PathError::ParentComponent => "'..' is not allowed".into(),
            PathError::ControlCharacter => "path contains a control character".into(),
            PathError::IllegalCharacter(c) => format!("'{c}' is not allowed in a name"),
            PathError::LeadingDot => "names cannot start with '.'".into(),
            PathError::LeadingOrTrailingSpace => "names cannot start or end with a space".into(),
            PathError::TrailingDot => "names cannot end with '.'".into(),
            PathError::ComponentTooLong => "a name in this path is too long".into(),
            PathError::ReservedName => "that name is reserved by some operating systems".into(),
            PathError::NotMarkdown => "notes must end in '.md'".into(),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for PathError {}

/// Characters that are legal on Linux but break on Windows and macOS. Rejecting
/// them keeps a vault portable, which matters because the whole point of storing
/// notes as plain files is that other tools can read them.
const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Device names DOS reserved, which Windows still honours. A file called `con.md`
/// cannot be created there, so we refuse to create one here.
const RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validates a single path component (one file or folder name, no separators).
pub fn validate_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::ComponentTooLong);
    }
    if component == "." {
        return Err(PathError::DotComponent);
    }
    if component == ".." {
        return Err(PathError::ParentComponent);
    }

    for ch in component.chars() {
        if ch.is_control() {
            return Err(PathError::ControlCharacter);
        }
        if ILLEGAL_CHARS.contains(&ch) {
            return Err(PathError::IllegalCharacter(ch));
        }
        if ch == '/' {
            return Err(PathError::EmptyComponent);
        }
        if ch == '\\' {
            return Err(PathError::Backslash);
        }
    }

    // A leading dot both hides the file and would let a user address `.trash`
    // or a `.git` directory, so it is refused outright rather than special-cased.
    if component.starts_with('.') {
        return Err(PathError::LeadingDot);
    }
    if component.starts_with(' ') || component.ends_with(' ') {
        return Err(PathError::LeadingOrTrailingSpace);
    }
    // Windows silently strips trailing dots, which would desynchronise the name
    // on disk from the name in Postgres.
    if component.ends_with('.') {
        return Err(PathError::TrailingDot);
    }

    let stem = component
        .split_once('.')
        .map(|(s, _)| s)
        .unwrap_or(component)
        .to_ascii_lowercase();
    if RESERVED_STEMS.contains(&stem.as_str()) {
        return Err(PathError::ReservedName);
    }

    Ok(())
}

/// Validates a vault-relative path that must name something (a note or a folder).
pub fn validate_rel_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }
    // Guard against a Windows-style drive prefix (`C:\notes`) arriving from a
    // client that built the path with native separators.
    if path.contains('\\') {
        return Err(PathError::Backslash);
    }

    for component in path.split('/') {
        validate_component(component)?;
    }
    Ok(())
}

/// Like [`validate_rel_path`], but the empty string is accepted as the vault root.
pub fn validate_folder_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Ok(());
    }
    validate_rel_path(path)
}

/// Validates a path that must be a markdown note.
pub fn validate_note_path(path: &str) -> Result<(), PathError> {
    validate_rel_path(path)?;
    if !has_md_extension(path) {
        return Err(PathError::NotMarkdown);
    }
    Ok(())
}

pub fn has_md_extension(path: &str) -> bool {
    let name = basename(path);
    name.len() > 3 && name.to_ascii_lowercase().ends_with(".md")
}

/// Last component of a path. Returns the whole string when there is no separator.
pub fn basename(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((_, name)) => name,
        None => path,
    }
}

/// Everything before the last separator. The empty string means the vault root.
pub fn parent_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

/// Filename with its extension removed.
pub fn stem(path: &str) -> &str {
    let name = basename(path);
    match name.rsplit_once('.') {
        // A name like `.env` is all extension and no stem; keep it whole.
        Some((s, _)) if !s.is_empty() => s,
        _ => name,
    }
}

/// Joins a parent path and a component, tolerating an empty parent (the root).
pub fn join(parent: &str, component: &str) -> String {
    if parent.is_empty() {
        component.to_string()
    } else {
        format!("{parent}/{component}")
    }
}

/// The title a note gets when its frontmatter does not supply one: the filename
/// without its extension, matching Obsidian's behaviour.
pub fn title_from_path(path: &str) -> &str {
    stem(path)
}

/// True when `path` is inside `folder` (at any depth). An empty `folder` is the
/// vault root and therefore contains everything.
pub fn is_within(path: &str, folder: &str) -> bool {
    if folder.is_empty() {
        return true;
    }
    path.len() > folder.len()
        && path.starts_with(folder)
        && path.as_bytes().get(folder.len()) == Some(&b'/')
}

/// Rewrites `path` for a move of `from` to `to`, where `path` is `from` itself
/// or something beneath it. Returns `None` when `path` is unaffected.
///
/// Used to update every descendant when a folder is renamed.
pub fn rebase(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        return Some(to.to_string());
    }
    if is_within(path, from) {
        // +1 skips the separator that `is_within` proved is present.
        return Some(join(to, &path[from.len() + 1..]));
    }
    None
}

/// Replaces characters that cannot appear in a filename so a user-supplied
/// title (or username, for a vault directory) can be used as one.
///
/// The result is guaranteed to pass [`validate_component`], falling back to
/// `fallback` when sanitising leaves nothing usable.
pub fn sanitize_component(input: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_control() || ILLEGAL_CHARS.contains(&ch) || ch == '/' || ch == '\\' {
            out.push('-');
        } else {
            out.push(ch);
        }
    }

    // Trim the characters that are legal mid-name but illegal at the edges.
    let trimmed = out.trim_matches(|c: char| c == ' ' || c == '.');
    let mut result = trimmed.to_string();

    if result.len() > MAX_COMPONENT_BYTES {
        // Truncate on a character boundary, not a byte one.
        let mut end = MAX_COMPONENT_BYTES;
        while end > 0 && !result.is_char_boundary(end) {
            end -= 1;
        }
        result.truncate(end);
        result = result.trim_end_matches([' ', '.']).to_string();
    }

    if validate_component(&result).is_err() {
        // Covers the empty case and reserved device names in one branch.
        if result.is_empty() {
            return fallback.to_string();
        }
        return format!("{result}-1");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The security-critical table: every one of these must be refused, because
    /// each is a way of naming a file outside the vault or a file the app owns.
    #[test]
    fn rejects_traversal_and_escapes() {
        let hostile = [
            "..",
            "../secret",
            "a/../../etc/passwd",
            "a/b/..",
            "./a",
            "a/./b",
            "/etc/passwd",
            "/",
            "//etc/passwd",
            "a//b",
            "a/",
            "",
            "..\\..\\windows",
            "C:\\notes\\a.md",
            "notes\\a.md",
            ".trash/deleted.md",
            ".git/config",
            ".env",
            "a/.git/config",
            "a\u{0}b",
            "a\nb",
            "a\rb",
            "a\tb",
            "\u{7f}evil",
            "con",
            "CON.md",
            "com1.md",
            "a/nul.md",
            "LPT9",
            "trailing.",
            "trailing ",
            " leading",
            "a/ b/c.md",
            "bad<name",
            "bad>name",
            "bad:name",
            "bad\"name",
            "bad|name",
            "bad?name",
            "bad*name",
        ];
        for case in hostile {
            assert!(
                validate_rel_path(case).is_err(),
                "expected {case:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_paths() {
        let ok = [
            "note.md",
            "Projects/Kitchen Reno.md",
            "a/b/c/d/e.md",
            "Ünïcödé note.md",
            "日本語/ノート.md",
            "note with. dots.md",
            "emoji 🎉.md",
            "attachments/2026/diagram-a1b2c3.png",
            "hyphen-and_underscore.md",
            "concert.md", // starts with "con" but is not the reserved name
            "console.md",
        ];
        for case in ok {
            assert!(
                validate_rel_path(case).is_ok(),
                "expected {case:?} to be accepted, got {:?}",
                validate_rel_path(case)
            );
        }
    }

    #[test]
    fn enforces_length_limits() {
        let long_component = "a".repeat(MAX_COMPONENT_BYTES + 1);
        assert_eq!(
            validate_rel_path(&long_component),
            Err(PathError::ComponentTooLong)
        );

        let at_limit = "a".repeat(MAX_COMPONENT_BYTES);
        assert!(validate_rel_path(&at_limit).is_ok());

        // Many legal components can still exceed the total path budget.
        let deep = vec!["dir"; 400].join("/");
        assert_eq!(validate_rel_path(&deep), Err(PathError::TooLong));
    }

    #[test]
    fn root_is_a_folder_but_not_a_note() {
        assert!(validate_folder_path("").is_ok());
        assert_eq!(validate_rel_path(""), Err(PathError::Empty));
        assert_eq!(validate_note_path(""), Err(PathError::Empty));
    }

    #[test]
    fn note_paths_require_md_extension() {
        assert!(validate_note_path("a.md").is_ok());
        assert!(validate_note_path("Folder/a.MD").is_ok());
        assert_eq!(validate_note_path("a.txt"), Err(PathError::NotMarkdown));
        assert_eq!(validate_note_path("a"), Err(PathError::NotMarkdown));
        // ".md" alone would be a dotfile whose stem is empty.
        assert!(validate_note_path(".md").is_err());
    }

    #[test]
    fn splits_paths() {
        assert_eq!(basename("a/b/c.md"), "c.md");
        assert_eq!(basename("c.md"), "c.md");
        assert_eq!(parent_of("a/b/c.md"), "a/b");
        assert_eq!(parent_of("c.md"), "");
        assert_eq!(stem("a/b/c.md"), "c");
        assert_eq!(stem("a/b/c.tar.gz"), "c.tar");
        assert_eq!(stem("noext"), "noext");
        assert_eq!(join("", "a.md"), "a.md");
        assert_eq!(join("a", "b.md"), "a/b.md");
    }

    #[test]
    fn containment_does_not_match_sibling_prefixes() {
        assert!(is_within("Projects/a.md", "Projects"));
        assert!(is_within("Projects/sub/a.md", "Projects"));
        assert!(is_within("anything", ""));
        // The bug this guards: "Projects2" starts with "Projects".
        assert!(!is_within("Projects2/a.md", "Projects"));
        assert!(!is_within("Projects", "Projects"));
        assert!(!is_within("Other/a.md", "Projects"));
    }

    #[test]
    fn rebase_moves_a_subtree() {
        assert_eq!(rebase("a.md", "a.md", "b.md"), Some("b.md".into()));
        assert_eq!(
            rebase("Old/deep/note.md", "Old", "New"),
            Some("New/deep/note.md".into())
        );
        assert_eq!(rebase("Old/note.md", "Old", ""), Some("note.md".into()));
        assert_eq!(rebase("Other/note.md", "Old", "New"), None);
        // Sibling prefix must not be dragged along with the move.
        assert_eq!(rebase("Older/note.md", "Old", "New"), None);
    }

    #[test]
    fn sanitize_always_yields_a_valid_component() {
        let cases = [
            ("Josh Owen", "Josh Owen"),
            ("josh/owen", "josh-owen"),
            ("../../etc", "-..-etc"), // separators become dashes, leading dots trimmed
            ("  .hidden.  ", "hidden"),
            ("", "fallback"),
            ("...", "fallback"),
            ("con", "con-1"),
            ("a<b>c", "a-b-c"),
        ];
        for (input, expected) in cases {
            let got = sanitize_component(input, "fallback");
            assert_eq!(got, expected, "sanitizing {input:?}");
            assert!(
                validate_component(&got).is_ok(),
                "sanitized {input:?} into invalid {got:?}"
            );
        }
    }

    /// Whatever we sanitise, the result must be usable as a filename — this is
    /// what stops a hostile OIDC `preferred_username` from picking the vault path.
    #[test]
    fn sanitize_is_total() {
        let inputs = [
            "../../../root".to_string(),
            "\u{0}\u{1}\u{2}".to_string(),
            "🎉🎉🎉".to_string(),
            "x".repeat(500),
            // Multi-byte characters straddling the truncation boundary.
            "é".repeat(300),
            "nul".to_string(),
            "....".to_string(),
            "  ".to_string(),
            "a".repeat(199),
        ];
        for input in inputs {
            let got = sanitize_component(&input, "user");
            assert!(
                validate_component(&got).is_ok(),
                "sanitizing {input:?} produced invalid {got:?}"
            );
        }
    }
}
