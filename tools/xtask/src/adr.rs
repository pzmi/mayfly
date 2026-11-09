//! `cargo xtask adr new` and `cargo xtask adr index [--fix]` (D4; from site-preview ADR 0081).
//!
//! Parallel lanes clashed on ADR numbers and merges duplicated index rows. `adr new` takes the
//! next free number (or the one an orchestrator reserved with `--number`), writes the record
//! from `doc/adr/template.md` and inserts its index row in order; `adr index --fix` rewrites
//! every row of `doc/adr/README.md` from the ADR files, so a stale, unsorted, duplicated or
//! conflicted index is repaired by a command, never by hand. `adr_files` holds the parser.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::adr_files::{ADR_DIR, ADR_INDEX, Adr, StatusKind, parse_status, read_adrs};
use crate::repo;

const TEMPLATE: &str = "doc/adr/template.md";
const TEMPLATE_TITLE: &str = "# NNNN. Title";
const TEMPLATE_DATE: &str = "Date: YYYY-MM-DD";
const TEMPLATE_STATUS: &str = "Proposed | Accepted | Rejected | Superseded by NNNN";
const CONFLICT_MARKERS: &[&str] = &["<<<<<<<", "|||||||", "=======", ">>>>>>>"];

/// What `adr new` was asked for.
pub struct NewAdr<'a> {
    /// The title, as it appears after `# NNNN. `.
    pub title: &'a str,
    /// A number reserved by an orchestrator; else the next after the highest in use.
    pub number: Option<u16>,
    /// `YYYY-MM-DD`; else today (UTC).
    pub date: Option<&'a str>,
    /// The text of the status section.
    pub status: &'a str,
}

/// `cargo xtask adr new`.
pub fn run_new(request: &NewAdr<'_>) -> i32 {
    let date = request.date.map_or_else(today, str::to_owned);
    report(
        create(&repo::root(), request, &date)
            .map(|file| format!("created {file} and its row in {ADR_INDEX}")),
    )
}

/// `cargo xtask adr index [--fix]`.
pub fn run_index(fix: bool) -> i32 {
    report(index_command(&repo::root(), fix))
}

/// What `adr index` does at `root`: check, or rewrite with `fix`.
fn index_command(root: &Path, fix: bool) -> Result<String, String> {
    if !fix {
        return check_index(root);
    }
    Ok(if fix_index(root)? {
        format!("rewrote the rows of {ADR_INDEX}")
    } else {
        format!("{ADR_INDEX} is up to date")
    })
}

/// Prints the outcome and returns the exit code.
fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(message) => {
            println!("adr: {message}");
            0
        }
        Err(err) => {
            eprintln!("adr: {err}");
            1
        }
    }
}

/// Creates the ADR file and its index row; returns the file's root-relative path.
pub(crate) fn create(root: &Path, request: &NewAdr<'_>, date: &str) -> Result<String, String> {
    let title = request.title.trim();
    if title.is_empty() || title.contains(['\n', '|']) {
        return Err(format!(
            "invalid title `{title}`: it must be one non-empty line without `|`"
        ));
    }
    if !is_date(date) {
        return Err(format!("invalid date `{date}`: use YYYY-MM-DD"));
    }
    let kind = parse_status(request.status)?;
    let slug = slug(title);
    if slug.is_empty() {
        return Err(format!(
            "title `{title}` has no letters or digits for a file name"
        ));
    }
    let (adrs, problems) = read_adrs(root);
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let index = fs::read_to_string(root.join(ADR_INDEX))
        .map_err(|e| format!("cannot read {ADR_INDEX}: {e}"))?;
    let used = used_numbers(&adrs, &index);
    let number = allocate(&used, request.number)?;
    let template = fs::read_to_string(root.join(TEMPLATE))
        .map_err(|e| format!("cannot read {TEMPLATE}: {e}"))?;
    let text = render_template(&template, number, title, date, request.status)?;
    let name = format!("{number:04}-{slug}.md");
    let file = format!("{ADR_DIR}/{name}");
    let row = render_row(&name, number, title, &index_status(&kind, request.status));
    let index = insert_row(&index, number, &row)?;
    let path = root.join(&file);
    if path.exists() {
        return Err(format!("{file} already exists"));
    }
    fs::write(&path, text).map_err(|e| format!("cannot write {file}: {e}"))?;
    fs::write(root.join(ADR_INDEX), index).map_err(|e| format!("cannot write {ADR_INDEX}: {e}"))?;
    Ok(file)
}

/// Rewrites the index rows from the ADR files; returns whether the file changed.
pub(crate) fn fix_index(root: &Path) -> Result<bool, String> {
    let index = fs::read_to_string(root.join(ADR_INDEX))
        .map_err(|e| format!("cannot read {ADR_INDEX}: {e}"))?;
    let fixed = fixed_index(root, &index)?;
    if fixed == index {
        return Ok(false);
    }
    fs::write(root.join(ADR_INDEX), fixed).map_err(|e| format!("cannot write {ADR_INDEX}: {e}"))?;
    Ok(true)
}

/// Compares the index with what `--fix` would write; the error names each differing row.
fn check_index(root: &Path) -> Result<String, String> {
    let index = fs::read_to_string(root.join(ADR_INDEX))
        .map_err(|e| format!("cannot read {ADR_INDEX}: {e}"))?;
    let fixed = fixed_index(root, &index)?;
    if fixed == index {
        return Ok(format!("{ADR_INDEX} is up to date"));
    }
    let old: BTreeSet<&str> = index.lines().collect();
    let new: BTreeSet<&str> = fixed.lines().collect();
    let mut lines = vec![format!(
        "{ADR_INDEX} is stale; `cargo xtask adr index --fix` rewrites it:"
    )];
    lines.extend(old.difference(&new).map(|l| format!("- {l}")));
    lines.extend(new.difference(&old).map(|l| format!("+ {l}")));
    Err(lines.join("\n"))
}

/// The index text with its table rows regenerated from the ADR files under `root`.
fn fixed_index(root: &Path, index: &str) -> Result<String, String> {
    let (adrs, problems) = read_adrs(root);
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let rows = adrs
        .iter()
        .map(row_of)
        .collect::<Result<Vec<String>, String>>()?;
    replace_rows(index, &rows)
}

/// The index row of one ADR file.
fn row_of(adr: &Adr) -> Result<String, String> {
    let name = adr.file.rsplit('/').next().unwrap_or(&adr.file);
    let title = adr
        .title
        .as_deref()
        .ok_or_else(|| format!("{}: no `# NNNN. Title` line", adr.file))?;
    let kind = adr
        .status
        .clone()
        .map_err(|e| format!("{}: {e}", adr.file))?;
    let number: u16 = adr
        .number
        .parse()
        .map_err(|_| format!("{}: bad number", adr.file))?;
    let text = adr.status_text.as_deref().unwrap_or_default();
    Ok(render_row(name, number, title, &index_status(&kind, text)))
}

fn render_row(name: &str, number: u16, title: &str, status: &str) -> String {
    format!("| [{number:04}]({name}) | {title} | {status} |")
}

/// The status column for a status section: its kind, then the ADRs that amend it
/// (`Amended by 0060, 0070`) and the dates of amendments that name no ADR (`Amended
/// 2026-10-13`). Only text outside parentheses and code spans, up to the end of the sentence
/// or a colon, counts as the list of amending ADRs, so a link target or an aside does not.
pub(crate) fn index_status(kind: &StatusKind, text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut amenders = BTreeSet::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("amended by ") {
        let start = from + at + "amended by ".len();
        let (list, end) = sentence_list(&text[start..]);
        amenders.extend(four_digit_words(&list));
        from = start + end;
    }
    let mut dates = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("amended ") {
        let start = from + at + "amended ".len();
        if let Some(date) = text.get(start..start + 10)
            && is_date(date)
            && !dates.contains(&date)
        {
            dates.push(date);
        }
        from = start;
    }
    let mut parts = vec![kind.shown()];
    if !amenders.is_empty() {
        let list: Vec<String> = amenders.into_iter().collect();
        parts.push(format!("Amended by {}", list.join(", ")));
    }
    parts.extend(dates.into_iter().map(|d| format!("Amended {d}")));
    parts.join(". ")
}

/// The text from the start of `rest` to the end of its sentence (`. `, `:`, `;` or the end)
/// with parenthesised and backticked parts left out; and the byte length consumed.
fn sentence_list(rest: &str) -> (String, usize) {
    let mut depth = 0usize;
    let mut code = false;
    let mut kept = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '`' => code = !code,
            '(' if !code => depth += 1,
            ')' if !code => depth = depth.saturating_sub(1),
            ':' | ';' if !code && depth == 0 => return (kept, i),
            '.' if !code
                && depth == 0
                && chars.peek().is_none_or(|(_, next)| next.is_whitespace()) =>
            {
                return (kept, i);
            }
            _ if !code && depth == 0 => kept.push(c),
            _ => {}
        }
    }
    (kept, rest.len())
}

fn four_digit_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() == 4 && w.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .collect()
}

/// Replaces the table rows (the lines after the `|---|` separator) with `rows`. Conflict
/// markers inside the table are dropped with the rows they bracket; a marker anywhere else is
/// an error, because only the rows are derived from the files.
fn replace_rows(index: &str, rows: &[String]) -> Result<String, String> {
    let lines: Vec<&str> = index.lines().collect();
    let separator = lines
        .iter()
        .position(|l| l.starts_with("|---") || l.starts_with("|:--"))
        .ok_or_else(|| format!("{ADR_INDEX}: no `| ADR | Title | Status |` table"))?;
    let start = separator + 1;
    let end = lines[start..]
        .iter()
        .position(|l| !(l.starts_with('|') || is_marker(l)))
        .map_or(lines.len(), |n| start + n);
    let mut out: Vec<&str> = lines[..start].to_vec();
    out.extend(rows.iter().map(String::as_str));
    out.extend(&lines[end..]);
    if let Some(line) = out.iter().find(|l| is_marker(l)) {
        return Err(format!(
            "{ADR_INDEX}: a conflict marker outside the table (`{line}`) must be resolved by hand"
        ));
    }
    let mut text = out.join("\n");
    if index.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn is_marker(line: &str) -> bool {
    CONFLICT_MARKERS.iter().any(|m| line.starts_with(m))
}

/// Inserts `row` before the first row with a higher number; a row with the same number is
/// an error.
fn insert_row(index: &str, number: u16, row: &str) -> Result<String, String> {
    let mut lines: Vec<&str> = index.lines().collect();
    let separator = lines
        .iter()
        .position(|l| l.starts_with("|---") || l.starts_with("|:--"))
        .ok_or_else(|| format!("{ADR_INDEX}: no `| ADR | Title | Status |` table"))?;
    let mut at = separator + 1;
    while let Some(line) = lines.get(at)
        && line.starts_with('|')
    {
        match row_number(line) {
            Some(n) if n == number => {
                return Err(format!("{ADR_INDEX} already has a row for {number:04}"));
            }
            Some(n) if n > number => break,
            _ => at += 1,
        }
    }
    lines.insert(at, row);
    let mut text = lines.join("\n");
    if index.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// The number of an index row `| [NNNN](...`.
fn row_number(line: &str) -> Option<u16> {
    let digits = line.strip_prefix("| [")?.get(..4)?;
    digits
        .chars()
        .all(|c| c.is_ascii_digit())
        .then(|| digits.parse().ok())?
}

/// Every number used by an ADR file or an index row (a row may survive its file in a merge).
fn used_numbers(adrs: &[Adr], index: &str) -> BTreeSet<u16> {
    adrs.iter()
        .filter_map(|a| a.number.parse().ok())
        .chain(index.lines().filter_map(row_number))
        .collect()
}

/// The reserved number when it is free, else the one after the highest in use. Gaps are not
/// reused: a gap may be a number another lane holds.
pub(crate) fn allocate(used: &BTreeSet<u16>, reserved: Option<u16>) -> Result<u16, String> {
    match reserved {
        Some(0) => Err("ADR numbers start at 1".to_owned()),
        Some(n) if used.contains(&n) => Err(format!("ADR {n:04} is already in use")),
        Some(n) => Ok(n),
        None => {
            let next = used.last().map_or(Some(1), |n| n.checked_add(1));
            next.filter(|n| *n <= 9999)
                .ok_or_else(|| "no ADR number left".to_owned())
        }
    }
}

/// The template with its title, date and status placeholders filled.
fn render_template(
    template: &str,
    number: u16,
    title: &str,
    date: &str,
    status: &str,
) -> Result<String, String> {
    for placeholder in [TEMPLATE_TITLE, TEMPLATE_DATE, TEMPLATE_STATUS] {
        if !template.contains(placeholder) {
            return Err(format!("{TEMPLATE} has no `{placeholder}` line"));
        }
    }
    Ok(template
        .replacen(TEMPLATE_TITLE, &format!("# {number:04}. {title}"), 1)
        .replacen(TEMPLATE_DATE, &format!("Date: {date}"), 1)
        .replacen(TEMPLATE_STATUS, status.trim(), 1))
}

/// The kebab-case file-name part of a title: lowercase ASCII letters and digits, every other
/// run of characters one hyphen.
pub(crate) fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn is_date(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Today's date (UTC) as `YYYY-MM-DD`.
fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_date(secs / 86_400)
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_date(days: u64) -> String {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE_TEXT: &str = "# NNNN. Title\n\nDate: YYYY-MM-DD\n\n## Status\n\nProposed | Accepted | Rejected | Superseded by NNNN\n\n## Context\n\nWhy.\n";

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn adr_text(number: &str, title: &str, status: &str) -> String {
        format!(
            "# {number}. {title}\n\nDate: 2026-01-01\n\n## Status\n\n{status}\n\n## Context\n\nx\n"
        )
    }

    /// A tree with the template, ADRs 0001 and 0003, and an index listing both.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), TEMPLATE, TEMPLATE_TEXT);
        write(
            dir.path(),
            "doc/adr/0001-a.md",
            &adr_text("0001", "A", "Accepted"),
        );
        write(
            dir.path(),
            "doc/adr/0003-c.md",
            &adr_text("0003", "C", "Proposed (planner default). Amended by 0004."),
        );
        write(
            dir.path(),
            ADR_INDEX,
            "# ADRs\n\nIntro.\n\n| ADR | Title | Status |\n|---|---|---|\n| [0001](0001-a.md) | A | Accepted |\n| [0003](0003-c.md) | C | Proposed. Amended by 0004 |\n\nAfter.\n",
        );
        dir
    }

    fn status(kind: StatusKind, text: &str) -> String {
        index_status(&kind, text)
    }

    #[test]
    fn should_show_only_the_kind_when_the_status_has_no_amendment() {
        // given
        let text = "Proposed (planner default — owner may override)";

        // when
        let shown = status(StatusKind::Proposed, text);

        // then
        assert_eq!(shown, "Proposed");
    }

    #[test]
    fn should_list_amending_adrs_when_the_status_names_them() {
        // given
        let cases = [
            (
                "Proposed. Amended by 0069 and 0072.",
                "Proposed. Amended by 0069, 0072",
            ),
            (
                "Proposed. Amended by ADR 0060 (the controls run in `x`, not 1234) and by ADR 0070 (three; more `a.rs`).",
                "Proposed. Amended by 0060, 0070",
            ),
            (
                "Proposed. The order is amended by [ADR 0067](0067-root.md): the 0048 start-up.",
                "Proposed. Amended by 0067",
            ),
            (
                "Accepted. Amended by ADR 0073: x. Amended by the OPS-04 review: y.",
                "Accepted. Amended by 0073",
            ),
            ("Proposed. Amends 0063. Extends 0072.", "Proposed"),
        ];

        // when
        let shown: Vec<String> = cases
            .iter()
            .map(|(text, _)| status(parse_status(text).unwrap(), text))
            .collect();

        // then
        let expected: Vec<&str> = cases.iter().map(|(_, want)| *want).collect();
        assert_eq!(shown, expected);
    }

    #[test]
    fn should_show_the_date_when_an_amendment_names_no_adr() {
        // given
        let text =
            "Proposed. Amended 2026-10-14 (review): stricter checks. Amended 2026-10-14 again.";

        // when
        let shown = status(StatusKind::Proposed, text);

        // then
        assert_eq!(shown, "Proposed. Amended 2026-10-14");
    }

    #[test]
    fn should_show_rejected_and_its_amendment_date_when_the_owner_declined_it() {
        // given
        let text = "Rejected (owner, 2026-11-09); amended 2026-11-10.";

        // when
        let shown = status(parse_status(text).unwrap(), text);

        // then
        assert_eq!(shown, "Rejected. Amended 2026-11-10");
    }

    #[test]
    fn should_show_the_successor_when_the_adr_is_superseded() {
        // given
        let text = "Superseded by [0032](0032-whole-plan-on-main.md)";

        // when
        let shown = status(parse_status(text).unwrap(), text);

        // then
        assert_eq!(shown, "Superseded by 0032");
    }

    #[test]
    fn should_make_a_kebab_slug_when_the_title_has_punctuation() {
        // given
        let titles = [
            "ADR allocation and landing command",
            "`check` composes: stages!",
            "--",
        ];

        // when
        let slugs: Vec<String> = titles.iter().map(|t| slug(t)).collect();

        // then
        assert_eq!(
            slugs,
            [
                "adr-allocation-and-landing-command",
                "check-composes-stages",
                ""
            ]
        );
    }

    #[test]
    fn should_take_the_next_number_after_the_highest_when_none_is_reserved() {
        // given
        let used: BTreeSet<u16> = [1, 2, 5].into_iter().collect();

        // when
        let next = allocate(&used, None);

        // then
        assert_eq!(next, Ok(6));
        assert_eq!(allocate(&BTreeSet::new(), None), Ok(1));
        assert!(allocate(&[9999].into_iter().collect(), None).is_err());
    }

    #[test]
    fn should_use_the_reserved_number_when_it_is_free() {
        // given
        let used: BTreeSet<u16> = [1, 2, 5].into_iter().collect();

        // when
        let results = [
            allocate(&used, Some(81)),
            allocate(&used, Some(5)),
            allocate(&used, Some(0)),
        ];

        // then
        assert_eq!(results[0], Ok(81));
        assert_eq!(results[1], Err("ADR 0005 is already in use".to_owned()));
        assert!(results[2].is_err());
    }

    #[test]
    fn should_render_dates_when_days_since_the_epoch_are_given() {
        // given
        let days = [0, 11_016, 20_740];

        // when
        let dates: Vec<String> = days.into_iter().map(civil_date).collect();

        // then
        assert_eq!(dates, ["1970-01-01", "2000-02-29", "2026-10-14"]);
        assert!(is_date(&today()));
    }

    #[test]
    fn should_create_the_file_and_a_sorted_row_when_a_new_adr_is_added() {
        // given
        let dir = tree();
        let request = NewAdr {
            title: "B decision",
            number: Some(2),
            date: None,
            status: "Accepted (owner, 2026-10-14)",
        };

        // when
        let file = create(dir.path(), &request, "2026-10-14").unwrap();

        // then
        assert_eq!(file, "doc/adr/0002-b-decision.md");
        let text = fs::read_to_string(dir.path().join(&file)).unwrap();
        assert!(text.starts_with("# 0002. B decision\n\nDate: 2026-10-14\n\n## Status\n\nAccepted (owner, 2026-10-14)\n"), "{text}");
        let index = fs::read_to_string(dir.path().join(ADR_INDEX)).unwrap();
        assert!(
            index.contains("| [0001](0001-a.md) | A | Accepted |\n| [0002](0002-b-decision.md) | B decision | Accepted |\n| [0003]"),
            "{index}"
        );
        assert_eq!(fix_index(dir.path()), Ok(false));
    }

    #[test]
    fn should_take_the_number_after_the_highest_when_no_number_is_reserved() {
        // given
        let dir = tree();
        let request = NewAdr {
            title: "D",
            number: None,
            date: None,
            status: "Proposed",
        };

        // when
        let file = create(dir.path(), &request, "2026-10-14");

        // then
        assert_eq!(file, Ok("doc/adr/0004-d.md".to_owned()));
        let index = fs::read_to_string(dir.path().join(ADR_INDEX)).unwrap();
        assert!(
            index.ends_with("| [0004](0004-d.md) | D | Proposed |\n\nAfter.\n"),
            "{index}"
        );
    }

    #[test]
    fn should_refuse_when_the_request_is_invalid() {
        // given
        let dir = tree();
        let ask = |title, number, status| NewAdr {
            title,
            number,
            date: None,
            status,
        };

        // when
        let results = [
            create(dir.path(), &ask("A | B", None, "Proposed"), "2026-10-14"),
            create(dir.path(), &ask("!!", None, "Proposed"), "2026-10-14"),
            create(dir.path(), &ask("E", Some(3), "Proposed"), "2026-10-14"),
            create(dir.path(), &ask("E", None, "Draft"), "2026-10-14"),
            create(dir.path(), &ask("E", None, "Proposed"), "14.10.2026"),
        ];

        // then
        assert!(results.iter().all(Result::is_err), "{results:?}");
        assert_eq!(results[2], Err("ADR 0003 is already in use".to_owned()));
        assert!(!dir.path().join("doc/adr/0004-e.md").exists());
    }

    #[test]
    fn should_refuse_when_the_template_lacks_a_placeholder() {
        // given
        let template =
            "# NNNN. Title\n\n## Status\n\nProposed | Accepted | Rejected | Superseded by NNNN\n";

        // when
        let rendered = render_template(template, 1, "T", "2026-10-14", "Proposed");

        // then
        assert_eq!(
            rendered,
            Err(format!("{TEMPLATE} has no `Date: YYYY-MM-DD` line"))
        );
    }

    #[test]
    fn should_sort_dedup_and_rederive_rows_when_the_index_is_fixed() {
        // given
        let dir = tree();
        write(
            dir.path(),
            ADR_INDEX,
            "# ADRs\n\n| ADR | Title | Status |\n|---|---|---|\n| [0003](0003-c.md) | Old | Proposed |\n| [0001](0001-a.md) | A | Accepted |\n| [0001](0001-a.md) | A | Accepted |\n| [0009](0009-gone.md) | Gone | Accepted |\n",
        );

        // when
        let changed = fix_index(dir.path());

        // then
        assert_eq!(changed, Ok(true));
        assert_eq!(
            fs::read_to_string(dir.path().join(ADR_INDEX)).unwrap(),
            "# ADRs\n\n| ADR | Title | Status |\n|---|---|---|\n| [0001](0001-a.md) | A | Accepted |\n| [0003](0003-c.md) | C | Proposed. Amended by 0004 |\n"
        );
        assert_eq!(check_index(dir.path()).map(|_| ()), Ok(()));
    }

    #[test]
    fn should_resolve_a_conflicted_table_when_the_index_is_fixed() {
        // given
        let dir = tree();
        write(
            dir.path(),
            "doc/adr/0002-b.md",
            &adr_text("0002", "B", "Proposed"),
        );
        write(
            dir.path(),
            ADR_INDEX,
            "# ADRs\n\n| ADR | Title | Status |\n|---|---|---|\n| [0001](0001-a.md) | A | Accepted |\n<<<<<<< HEAD\n| [0003](0003-c.md) | C | Proposed |\n=======\n| [0002](0002-b.md) | B | Proposed |\n>>>>>>> lane\n\nAfter.\n",
        );

        // when
        let before = check_index(dir.path());
        let changed = fix_index(dir.path());

        // then
        assert!(before.unwrap_err().contains("- <<<<<<< HEAD"));
        assert_eq!(changed, Ok(true));
        let index = fs::read_to_string(dir.path().join(ADR_INDEX)).unwrap();
        assert!(index.contains("|---|---|---|\n| [0001](0001-a.md) | A | Accepted |\n| [0002](0002-b.md) | B | Proposed |\n| [0003](0003-c.md) | C | Proposed. Amended by 0004 |\n\nAfter.\n"), "{index}");
    }

    #[test]
    fn should_report_success_or_failure_when_the_index_command_runs() {
        // given
        let dir = tree();
        write(
            dir.path(),
            "doc/adr/0002-b.md",
            &adr_text("0002", "B", "Proposed"),
        );

        // when
        let stale = index_command(dir.path(), false);
        let fixed = index_command(dir.path(), true);
        let again = index_command(dir.path(), true);

        // then
        assert!(stale.as_ref().unwrap_err().contains("is stale"));
        assert_eq!(fixed, Ok(format!("rewrote the rows of {ADR_INDEX}")));
        assert_eq!(again, Ok(format!("{ADR_INDEX} is up to date")));
        assert_eq!((report(again), report(stale)), (0, 1));
    }

    #[test]
    fn should_refuse_to_fix_when_a_conflict_is_outside_the_table() {
        // given
        let dir = tree();
        write(
            dir.path(),
            ADR_INDEX,
            "# ADRs\n\n<<<<<<< HEAD\nOne.\n=======\nTwo.\n>>>>>>> lane\n\n| ADR | Title | Status |\n|---|---|---|\n",
        );

        // when
        let fixed = fix_index(dir.path());

        // then
        assert!(
            fixed
                .unwrap_err()
                .contains("conflict marker outside the table")
        );
    }

    #[test]
    fn should_refuse_when_the_index_has_no_table_or_an_adr_is_malformed() {
        // given
        let no_table = tree();
        write(no_table.path(), ADR_INDEX, "# ADRs\n");
        let malformed = tree();
        write(malformed.path(), "doc/adr/0002-b.md", "no title\n");

        // when
        let results = [fix_index(no_table.path()), fix_index(malformed.path())];

        // then
        assert!(
            results[0]
                .as_ref()
                .unwrap_err()
                .contains("no `| ADR | Title | Status |` table")
        );
        assert!(results[1].as_ref().unwrap_err().contains("0002-b.md"));
    }
}
