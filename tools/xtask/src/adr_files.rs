//! The ADR files under `doc/adr/`: their names, title lines and status sections, read by
//! `cargo xtask adr` and by `land` (which repairs a conflicted index with `adr index --fix`).
//!
//! Ported from site-preview's `docs` stage, which also checked the index against the files;
//! here `adr index` (without `--fix`) is that check: it compares the index with what `--fix`
//! would write.

use std::fs;
use std::path::Path;

/// The ADR directory, relative to the repository root.
pub(crate) const ADR_DIR: &str = "doc/adr";
/// The ADR index, relative to the repository root.
pub(crate) const ADR_INDEX: &str = "doc/adr/README.md";

/// Directory names never searched: build output, dependencies, work state.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules", ".work"];

/// The kinds of ADR status (ADR 0001, amended 2026-11-09). What follows the kind (`(owner,
/// 2026-11-09)`, `(planner default)`, `. Amended by 0069`) is annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Proposed,
    Accepted,
    /// Considered and declined; kept so it is not proposed again.
    Rejected,
    Superseded(String),
}

impl StatusKind {
    pub(crate) fn shown(&self) -> String {
        match self {
            Self::Proposed => "Proposed".to_owned(),
            Self::Accepted => "Accepted".to_owned(),
            Self::Rejected => "Rejected".to_owned(),
            Self::Superseded(by) => format!("Superseded by {by}"),
        }
    }
}

/// Parses the leading status kind of `text`.
pub(crate) fn parse_status(text: &str) -> Result<StatusKind, String> {
    let text = text.trim();
    let boundary = |rest: &str| !rest.starts_with(|c: char| c.is_alphanumeric());
    for (word, kind) in [
        ("Proposed", StatusKind::Proposed),
        ("Accepted", StatusKind::Accepted),
        ("Rejected", StatusKind::Rejected),
    ] {
        if let Some(rest) = text.strip_prefix(word)
            && boundary(rest)
        {
            return Ok(kind);
        }
    }
    if let Some(rest) = text.strip_prefix("Superseded by ") {
        let rest = rest.strip_prefix('[').unwrap_or(rest);
        let number: String = rest.chars().take(4).collect();
        if number.len() == 4 && number.chars().all(|c| c.is_ascii_digit()) && boundary(&rest[4..]) {
            return Ok(StatusKind::Superseded(number));
        }
    }
    Err(format!(
        "invalid status `{text}`: it must start with `Proposed`, `Accepted`, `Rejected` or `Superseded by NNNN`"
    ))
}

/// One ADR file.
pub(crate) struct Adr {
    /// Root-relative path, `doc/adr/NNNN-slug.md`.
    pub(crate) file: String,
    /// The four digits of the file name.
    pub(crate) number: String,
    /// The title after `# NNNN. `, when the first line has that shape.
    pub(crate) title: Option<String>,
    /// The leading status kind of the status section.
    pub(crate) status: Result<StatusKind, String>,
    /// The first paragraph of the status section, joined into one line (`adr index`).
    pub(crate) status_text: Option<String>,
}

/// Reads `doc/adr/NNNN-*.md`: file name, title line and status section.
pub(crate) fn read_adrs(root: &Path) -> (Vec<Adr>, Vec<String>) {
    let mut problems = Vec::new();
    let mut files = Vec::new();
    walk(
        root,
        &root.join(ADR_DIR),
        &mut |rel| {
            rel.ends_with(".md")
                && rel.matches('/').count() == 2
                && !rel.ends_with("/README.md")
                && !rel.ends_with("/template.md")
        },
        &mut files,
    );
    files.sort();
    let mut adrs = Vec::new();
    for file in files {
        let name = file.rsplit('/').next().unwrap_or(&file);
        let valid = name.len() > 8
            && name[..4].chars().all(|c| c.is_ascii_digit())
            && name[4..].starts_with('-')
            && name[5..name.len() - 3]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            problems.push(format!(
                "{file}:1: ADR file names are `NNNN-kebab-title.md`"
            ));
            continue;
        }
        let text = fs::read_to_string(root.join(&file)).unwrap_or_default();
        let title_line = text.lines().next().unwrap_or_default();
        let number = name[..4].to_owned();
        let title = match title_line
            .strip_prefix("# ")
            .and_then(|t| t.split_once(". "))
        {
            Some((n, title)) if n == number => Some(title.trim().to_owned()),
            Some((n, _)) => {
                problems.push(format!(
                    "{file}:1: title says ADR {n} but the file name says {number}"
                ));
                None
            }
            None => {
                problems.push(format!(
                    "{file}:1: the first line must be `# {number}. Title`"
                ));
                None
            }
        };
        let status_text = status_section(&text);
        let status = status_text
            .clone()
            .ok_or_else(|| "no `## Status` section with text".to_owned())
            .and_then(|s| parse_status(&s));
        if let Err(err) = &status {
            problems.push(format!("{file}:1: {err}"));
        }
        adrs.push(Adr {
            file,
            number,
            title,
            status,
            status_text,
        });
    }
    (adrs, problems)
}

/// The first paragraph under `## Status`.
fn status_section(text: &str) -> Option<String> {
    let mut lines = text.lines().skip_while(|l| l.trim() != "## Status");
    lines.next()?;
    let paragraph: Vec<&str> = lines
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::trim)
        .collect();
    (!paragraph.is_empty()).then(|| paragraph.join(" "))
}

/// Collects root-relative files for which `keep` is true.
fn walk(root: &Path, dir: &Path, keep: &mut dyn FnMut(&str) -> bool, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                walk(root, &path, keep, out);
            }
        } else if kind.is_file() {
            let rel = relative(root, &path);
            if keep(&rel) {
                out.push(rel);
            }
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_annotated_statuses_when_the_kind_leads() {
        // given
        let texts = [
            "Proposed (planner default \u{2014} owner may override). Amended by ADR 0054",
            "Accepted",
            "Superseded by [0032](0032-x.md)",
            "Superseded by 0032",
        ];

        // when
        let kinds: Vec<_> = texts.iter().map(|t| parse_status(t)).collect();

        // then
        assert_eq!(kinds[0], Ok(StatusKind::Proposed));
        assert_eq!(kinds[1], Ok(StatusKind::Accepted));
        assert_eq!(kinds[2], Ok(StatusKind::Superseded("0032".into())));
        assert_eq!(kinds[3], Ok(StatusKind::Superseded("0032".into())));
        assert!(parse_status("Acceptedish").is_err());
        assert!(parse_status("Superseded by the next one").is_err());
    }

    #[test]
    fn should_accept_rejected_and_decider_annotations_when_the_kind_leads() {
        // given
        let texts = [
            "Rejected (owner, 2026-11-09)",
            "Rejected (owner, 2026-11-09); amended 2026-11-10.",
            "Accepted (owner, 2026-11-09); amended 2026-11-09.",
        ];

        // when
        let kinds: Vec<_> = texts.iter().map(|t| parse_status(t)).collect();

        // then
        assert_eq!(
            kinds,
            [
                Ok(StatusKind::Rejected),
                Ok(StatusKind::Rejected),
                Ok(StatusKind::Accepted)
            ]
        );
    }

    #[test]
    fn should_refuse_a_misspelt_status_when_it_only_resembles_a_kind() {
        // given
        let texts = ["Rejectd (owner, 2026-11-09)", "Rejectedish", "rejected"];

        // when
        let kinds: Vec<_> = texts.iter().map(|t| parse_status(t)).collect();

        // then
        assert!(kinds.iter().all(Result::is_err), "{kinds:?}");
        assert!(
            kinds[0].as_ref().is_err_and(|e| e.contains("`Rejected`")),
            "{kinds:?}"
        );
    }

    #[test]
    fn should_report_misnamed_file_when_adr_name_is_not_kebab_case() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let adr = dir.path().join(ADR_DIR);
        fs::create_dir_all(&adr).unwrap();
        fs::write(adr.join("0001-Bad_Name.md"), "# 0001. Bad\n").unwrap();
        fs::write(
            adr.join("0002-good.md"),
            "# 0002. Good\n\n## Status\n\nAccepted\n",
        )
        .unwrap();

        // when
        let (adrs, problems) = read_adrs(dir.path());

        // then
        assert_eq!(adrs.len(), 1);
        assert_eq!(adrs[0].title.as_deref(), Some("Good"));
        assert_eq!(
            problems,
            ["doc/adr/0001-Bad_Name.md:1: ADR file names are `NNNN-kebab-title.md`"]
        );
    }
}
