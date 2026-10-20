//! Mechanical checks for the AGENTS.md "Proportionality" rules that can be
//! verified by text scanning (std only, no parsing).
//!
//! * Rule 7: non-test source files stay under [`MAX_FILE_LINES`] lines.
//! * Rule 3: no new `pub const *_SCHEMA*/*_VERSION*: u16|u32` and no new
//!   `b"spall.` hash-domain literals.
//! * Rule 9: no tracked `*.jsonl` / `*.log` under `docs/`.
//! * Rule 5 (warning only): `pub mod fixtures|fault|proxy|capture` without a
//!   `cfg` attribute.
//!
//! Existing violations are grandfathered by `proportionality-allowlist.txt`,
//! which may only shrink. `cargo xtask proportionality --print-allowlist`
//! prints a fresh allowlist for the coordinator to review; agents must not
//! use it to add entries.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

const MAX_FILE_LINES: usize = 2_000;
const ALLOWLIST_PATH: &str = "tools/xtask/proportionality-allowlist.txt";
const SCAN_ROOTS: [&str; 3] = ["crates/", "examples/", "tools/"];
/// This file quotes the patterns it scans for; it is not scanned itself.
const SELF_PATH: &str = "tools/xtask/src/proportionality.rs";

/// Grandfathered state parsed from the allowlist file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Allowlist {
    /// path -> recorded line count
    lines: BTreeMap<String, usize>,
    /// (path, constant name)
    consts: BTreeSet<(String, String)>,
    /// (path, literal contents after `b"`)
    domains: BTreeSet<(String, String)>,
    /// tracked docs evidence files
    logs: BTreeSet<String>,
}

pub fn parse_allowlist(text: &str) -> Result<Allowlist, String> {
    let mut list = Allowlist::default();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("{ALLOWLIST_PATH}:{}: cannot parse {raw:?}", index + 1);
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("lines"), Some(path), Some(count), None) => {
                let count = count.parse().map_err(|_| bad())?;
                list.lines.insert(path.to_owned(), count);
            }
            (Some("const"), Some(path), Some(name), None) => {
                list.consts.insert((path.to_owned(), name.to_owned()));
            }
            (Some("domain"), Some(path), Some(literal), None) => {
                list.domains.insert((path.to_owned(), literal.to_owned()));
            }
            (Some("log"), Some(path), None, None) => {
                list.logs.insert(path.to_owned());
            }
            _ => return Err(bad()),
        }
    }
    Ok(list)
}

/// True for files rule 7/3 do not apply to (integration tests, benches).
fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/').any(|c| c == "tests" || c == "benches")
        || name == "tests.rs"
        || name.ends_with("_tests.rs")
}

/// Counting rule: every line of the file up to (not including) a
/// `#[cfg(test)]` attribute that is followed by a `mod` item. Everything from
/// that attribute on is the trailing test block and is not counted.
pub fn non_test_lines(source: &str) -> usize {
    let lines: Vec<&str> = source.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let next = lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with("#["));
        if let Some(next) = next
            && (next.starts_with("mod ") || next.starts_with("pub mod "))
        {
            return i;
        }
    }
    lines.len()
}

/// The non-test prefix of `source` (same cut as [`non_test_lines`]).
fn non_test_prefix(source: &str) -> impl Iterator<Item = &str> {
    source.lines().take(non_test_lines(source))
}

/// `(NAME)` of each `pub const *SCHEMA*|*VERSION*: u16|u32` in the non-test part.
pub fn scan_version_consts(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in non_test_prefix(source) {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, ty)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let ty = ty.trim_start();
        let is_int = ["u16", "u32"].iter().any(|t| {
            ty.strip_prefix(t)
                .is_some_and(|r| !r.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        });
        if is_int && (name.contains("SCHEMA") || name.contains("VERSION")) {
            found.push(name.to_owned());
        }
    }
    found
}

/// Contents of each `b"spall.…"` literal (up to the closing quote).
pub fn scan_domain_literals(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in non_test_prefix(source) {
        let mut rest = line;
        while let Some(at) = rest.find("b\"spall.") {
            let body = &rest[at + 2..];
            let end = body.find('"').unwrap_or(body.len());
            found.push(body[..end].to_owned());
            rest = &body[end..];
        }
    }
    found
}

/// Line numbers (1-based) of `pub mod fixtures|fault|proxy|capture` with no
/// `cfg` attribute among the contiguous attribute lines directly above.
pub fn scan_ungated_scaffolding(source: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("pub mod ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !["fixtures", "fault", "proxy", "capture"].contains(&name.as_str()) {
            continue;
        }
        let mut gated = false;
        for above in lines[..i].iter().rev() {
            let above = above.trim();
            if above.starts_with("#[") || above.starts_with("///") || above.starts_with("//") {
                gated |= above.contains("cfg");
            } else {
                break;
            }
        }
        if !gated {
            found.push((i + 1, name));
        }
    }
    found
}

/// Everything the check observed, independent of the allowlist.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Observed {
    pub lines: BTreeMap<String, usize>,
    pub consts: BTreeSet<(String, String)>,
    pub domains: BTreeSet<(String, String)>,
    pub logs: BTreeSet<String>,
    pub ungated: Vec<(String, usize, String)>,
}

/// The two sanctioned pre-release version numbers (and their direct aliases):
/// rule 3 allows these wherever they live, so moving one is not a violation.
const SANCTIONED_VERSIONS: &[&str] = &[
    "PROTOCOL_VERSION",
    "WIRE_SCHEMA_VERSION",
    "STORE_SCHEMA_VERSION",
    "STORE_SCHEMA_VERSION_U16",
];

/// Compares observations with the allowlist. Returns `(failures, warnings)`.
pub fn evaluate(observed: &Observed, allow: &Allowlist) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    for (path, &count) in &observed.lines {
        if count <= MAX_FILE_LINES && !allow.lines.contains_key(path) {
            continue;
        }
        match allow.lines.get(path) {
            None => failures.push(format!(
                "AGENTS.md Proportionality rule 7: {path} has {count} non-test lines (limit \
                 {MAX_FILE_LINES}). Split it (moves only) instead of growing it."
            )),
            Some(&recorded) if count > recorded => failures.push(format!(
                "AGENTS.md Proportionality rule 7: {path} grew to {count} non-test lines \
                 (allowlisted at {recorded}). Allowlisted files may not grow; split it first."
            )),
            Some(_) => {}
        }
    }
    for (path, name) in &observed.consts {
        if !SANCTIONED_VERSIONS.contains(&name.as_str())
            && !allow.consts.contains(&(path.clone(), name.clone()))
        {
            failures.push(format!(
                "AGENTS.md Proportionality rule 3: new version/schema constant `{name}` in \
                 {path}. Pre-release there is one PROTOCOL_VERSION and one STORE_SCHEMA_VERSION; \
                 bump those instead."
            ));
        }
    }
    for (path, literal) in &observed.domains {
        if !allow.domains.contains(&(path.clone(), literal.clone())) {
            failures.push(format!(
                "AGENTS.md Proportionality rule 3: new hash-domain literal b\"{literal}\" in \
                 {path}. Do not add new hash-domain strings pre-release."
            ));
        }
    }
    for path in &observed.logs {
        if !allow.logs.contains(path) {
            failures.push(format!(
                "AGENTS.md Proportionality rule 9: tracked evidence file {path}. Commit a short \
                 summary under docs/reports/ and keep raw *.jsonl/*.log out of git."
            ));
        }
    }
    let warnings = observed
        .ungated
        .iter()
        .map(|(path, line, name)| {
            format!(
                "AGENTS.md Proportionality rule 5 (warning): {path}:{line} `pub mod {name}` has \
                 no cfg gate; scaffolding belongs behind a cargo feature or in a test crate."
            )
        })
        .collect();
    (failures, warnings)
}

/// Renders an allowlist that exactly grandfathers `observed`.
pub fn render_allowlist(observed: &Observed) -> String {
    let mut out = String::from(
        "# Proportionality allowlist (checked by `cargo xtask check`).\n\
         # Entries may only shrink or be removed, never added, without user approval.\n\
         # Regenerate for review with `cargo xtask proportionality --print-allowlist`.\n\
         #\n\
         # lines <path> <count>      rule 7: non-test line count may not grow past <count>\n\
         # const <path> <NAME>       rule 3: existing *_SCHEMA*/*_VERSION*: u16|u32 constant\n\
         # domain <path> <literal>   rule 3: existing b\"spall.…\" hash-domain literal\n\
         # log <path>                rule 9: tracked docs *.jsonl/*.log\n\n",
    );
    for (path, &count) in &observed.lines {
        if count > MAX_FILE_LINES {
            out.push_str(&format!("lines {path} {count}\n"));
        }
    }
    for (path, name) in &observed.consts {
        if !SANCTIONED_VERSIONS.contains(&name.as_str()) {
            out.push_str(&format!("const {path} {name}\n"));
        }
    }
    for (path, literal) in &observed.domains {
        out.push_str(&format!("domain {path} {literal}\n"));
    }
    for path in &observed.logs {
        out.push_str(&format!("log {path}\n"));
    }
    out
}

fn tracked_files(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git ls-files: {e}"))?;
    if !output.status.success() {
        return Err(format!("git ls-files failed with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn observe(root: &Path) -> Result<Observed, String> {
    let mut observed = Observed::default();
    for path in tracked_files(root)? {
        if path.starts_with("docs/") && (path.ends_with(".jsonl") || path.ends_with(".log")) {
            observed.logs.insert(path.clone());
        }
        if !path.ends_with(".rs")
            || path == SELF_PATH
            || !SCAN_ROOTS.iter().any(|r| path.starts_with(r))
        {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        for (line, name) in scan_ungated_scaffolding(&source) {
            observed.ungated.push((path.clone(), line, name));
        }
        if is_test_path(&path) {
            continue;
        }
        observed.lines.insert(path.clone(), non_test_lines(&source));
        for name in scan_version_consts(&source) {
            observed.consts.insert((path.clone(), name));
        }
        for literal in scan_domain_literals(&source) {
            observed.domains.insert((path.clone(), literal));
        }
    }
    Ok(observed)
}

/// Runs the checks; `Err` carries the failure count message.
pub fn run(root: &Path, print_allowlist: bool) -> Result<(), String> {
    let observed = observe(root)?;
    if print_allowlist {
        print!("{}", render_allowlist(&observed));
        return Ok(());
    }
    let text = std::fs::read_to_string(root.join(ALLOWLIST_PATH))
        .map_err(|e| format!("cannot read {ALLOWLIST_PATH}: {e}"))?;
    let allow = parse_allowlist(&text)?;
    let (failures, warnings) = evaluate(&observed, &allow);
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    for failure in &failures {
        eprintln!("error: {failure}");
    }
    if failures.is_empty() {
        println!(
            "proportionality: ok ({} files scanned, {} warnings)",
            observed.lines.len(),
            warnings.len()
        );
        Ok(())
    } else {
        Err(format!(
            "proportionality: {} violation(s); see AGENTS.md Proportionality. The allowlist \
             ({ALLOWLIST_PATH}) may only shrink without user approval.",
            failures.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_stops_at_trailing_test_mod() {
        let src = "fn a() {}\nfn b() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        assert_eq!(non_test_lines(src), 3);
        // cfg(test) on a non-mod item does not cut the count.
        let src = "#[cfg(test)]\nfn helper() {}\nfn b() {}\n";
        assert_eq!(non_test_lines(src), 3);
        // Extra attributes between cfg(test) and mod still cut.
        let src = "fn a() {}\n#[cfg(test)]\n#[allow(dead_code)]\nmod tests {}\n";
        assert_eq!(non_test_lines(src), 1);
    }

    #[test]
    fn version_consts_match_only_schema_or_version_ints() {
        let src = "pub const FOO_SCHEMA_VERSION: u32 = 1;\n\
                   pub const BAR_VERSION: u16 = 2;\n\
                   pub const WIDE_VERSION: u64 = 3;\n\
                   pub const NAME_VERSION: &str = \"x\";\n\
                   pub const LIMIT: u32 = 4;\n\
                   const PRIVATE_VERSION: u32 = 5;\n\
                   #[cfg(test)]\nmod tests {\n    pub const T_VERSION: u32 = 1;\n}\n";
        assert_eq!(
            scan_version_consts(src),
            vec!["FOO_SCHEMA_VERSION", "BAR_VERSION"]
        );
    }

    #[test]
    fn domain_literals_are_extracted() {
        let src = "const A: &[u8] = b\"spall.brick.v1\";\nlet x = (b\"spall.a\", b\"spall.b\");\nlet y = \"spall.not-bytes\";\n";
        assert_eq!(
            scan_domain_literals(src),
            vec!["spall.brick.v1", "spall.a", "spall.b"]
        );
    }

    #[test]
    fn ungated_scaffolding_respects_cfg_attributes() {
        let src = "pub mod fixtures;\n\
                   #[cfg(feature = \"fixtures\")]\npub mod fault;\n\
                   #[cfg(feature = \"x\")]\n#[doc(hidden)]\npub mod proxy;\n\
                   pub mod other;\n\
                   pub mod capture {\n}\n";
        assert_eq!(
            scan_ungated_scaffolding(src),
            vec![(1, "fixtures".to_owned()), (8, "capture".to_owned())]
        );
    }

    #[test]
    fn test_paths_are_excluded() {
        assert!(is_test_path("crates/x/tests/a.rs"));
        assert!(is_test_path("crates/x/src/tests.rs"));
        assert!(is_test_path("crates/x/src/foo_tests.rs"));
        assert!(!is_test_path("crates/x/src/lib.rs"));
    }

    fn allow() -> Allowlist {
        parse_allowlist(
            "# c\nlines a.rs 2500\nconst a.rs OLD_VERSION\ndomain a.rs spall.old\nlog docs/x.jsonl\n",
        )
        .unwrap()
    }

    #[test]
    fn allowlist_parse_rejects_garbage() {
        assert!(parse_allowlist("lines a.rs notanumber\n").is_err());
        assert!(parse_allowlist("bogus a b\n").is_err());
    }

    #[test]
    fn evaluate_passes_grandfathered_state() {
        let mut o = Observed::default();
        o.lines.insert("a.rs".into(), 2500);
        o.lines.insert("b.rs".into(), 1999);
        o.consts.insert(("a.rs".into(), "OLD_VERSION".into()));
        o.domains.insert(("a.rs".into(), "spall.old".into()));
        o.logs.insert("docs/x.jsonl".into());
        let (failures, warnings) = evaluate(&o, &allow());
        assert!(failures.is_empty(), "{failures:?}");
        assert!(warnings.is_empty());
    }

    #[test]
    fn sanctioned_versions_may_live_anywhere() {
        let mut o = Observed::default();
        o.consts
            .insert(("moved.rs".into(), "STORE_SCHEMA_VERSION".into()));
        o.consts
            .insert(("moved.rs".into(), "PROTOCOL_VERSION".into()));
        let (failures, _) = evaluate(&o, &allow());
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn evaluate_flags_growth_new_files_and_new_names() {
        let mut o = Observed::default();
        o.lines.insert("a.rs".into(), 2501);
        o.lines.insert("b.rs".into(), 2001);
        o.consts.insert(("a.rs".into(), "NEW_VERSION".into()));
        o.domains.insert(("b.rs".into(), "spall.new".into()));
        o.logs.insert("docs/y.log".into());
        o.ungated.push(("c.rs".into(), 3, "fault".into()));
        let (failures, warnings) = evaluate(&o, &allow());
        assert_eq!(failures.len(), 5, "{failures:?}");
        assert!(
            failures
                .iter()
                .any(|f| f.contains("rule 7") && f.contains("grew"))
        );
        assert!(failures.iter().any(|f| f.contains("rule 9")));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("rule 5"));
    }

    #[test]
    fn render_then_parse_round_trips_and_passes() {
        let mut o = Observed::default();
        o.lines.insert("a.rs".into(), 2100);
        o.lines.insert("small.rs".into(), 10);
        o.consts.insert(("a.rs".into(), "X_VERSION".into()));
        o.domains.insert(("a.rs".into(), "spall.d".into()));
        o.logs.insert("docs/z.log".into());
        let parsed = parse_allowlist(&render_allowlist(&o)).unwrap();
        assert_eq!(parsed.lines.len(), 1);
        let (failures, _) = evaluate(&o, &parsed);
        assert!(failures.is_empty());
    }
}
