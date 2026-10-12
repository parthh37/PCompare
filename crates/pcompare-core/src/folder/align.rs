//! Alignment: which left entry lines up with which right entry.
//!
//! Entries pair when their *pairing keys* are equal. The key starts as the
//! name and is folded by the alignment options: Unicode normalization (NFC
//! and NFD spellings of `café` are one name), case, wildcard rules
//! (`Apples*` ⇒ `Bananas*` pairs on what the `*` matched) and extension
//! groups (`htm,html`) or ignoring extensions entirely.
//!
//! Control characters separate the parts of folded keys, so a folded key can
//! never collide with a real file name.

use super::Side;
use crate::{CoreError, Result};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};

/// One "align these names" override: `left` and `right` are masks where `*`
/// matches any run of characters and `?` one character.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlignRule {
    pub left: String,
    pub right: String,
}

/// How names are aligned beyond plain equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlignOptions {
    /// Pair names that differ only in Unicode normalization form (macOS
    /// stores decomposed NFD names, Windows and Linux usually composed NFC).
    pub unicode: bool,
    /// Pair files by name alone: `report.txt` lines up with `report.md`.
    pub ignore_extensions: bool,
    /// Groups of equivalent extensions, each a comma- or space-separated
    /// list such as `htm,html` or `.jpg .jpeg`.
    pub extension_groups: Vec<String>,
    /// Wildcard overrides, tried in order; the first matching mask wins.
    pub rules: Vec<AlignRule>,
}

impl Default for AlignOptions {
    fn default() -> Self {
        Self {
            unicode: true,
            ignore_extensions: false,
            extension_groups: Vec::new(),
            rules: Vec::new(),
        }
    }
}

/// Separates the parts of a folded key; never part of a file name.
const SEP: char = '\u{1}';
/// Marks a key whose extension was folded.
const EXT: char = '\u{2}';

/// Precompiled alignment options.
pub(crate) struct Aligner {
    ignore_case: bool,
    unicode: bool,
    ignore_extensions: bool,
    /// Lower-cased extension (no dot) → index of its group.
    groups: HashMap<String, usize>,
    /// `(left, right)` mask regexes; each `*`/`?` is a capture group.
    rules: Vec<(Regex, Regex)>,
}

/// Turns a `*`/`?` mask into an anchored regex with one group per wildcard.
fn mask_regex(mask: &str, ignore_case: bool) -> Result<Regex> {
    let mut pattern = String::from("^");
    for c in mask.chars() {
        match c {
            '*' => pattern.push_str("(.*?)"),
            '?' => pattern.push_str("(.)"),
            c => pattern.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    pattern.push('$');
    RegexBuilder::new(&pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| CoreError::BadPattern {
            pattern: mask.to_owned(),
            message: e.to_string(),
        })
}

fn nfc(s: &str) -> String {
    // Most names are already NFC; skip the allocation-heavy pass for them.
    match is_nfc_quick(s.chars()) {
        IsNormalized::Yes => s.to_owned(),
        _ => s.nfc().collect(),
    }
}

/// Splits `name.ext` into `(name, ext)`. Dot files (`.gitignore`) and names
/// ending in a dot have no extension.
fn split_ext(name: &str) -> Option<(&str, &str)> {
    let dot = name.rfind('.')?;
    (dot > 0 && dot + 1 < name.len()).then(|| (&name[..dot], &name[dot + 1..]))
}

impl Aligner {
    pub fn new(opts: &AlignOptions, ignore_case: bool) -> Result<Self> {
        let mut groups = HashMap::new();
        for (i, group) in opts.extension_groups.iter().enumerate() {
            for ext in group.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
                let ext = ext.trim().trim_start_matches('.');
                if !ext.is_empty() {
                    groups.insert(ext.to_lowercase(), i);
                }
            }
        }
        let prep = |mask: &str| {
            if opts.unicode {
                nfc(mask.trim())
            } else {
                mask.trim().to_owned()
            }
        };
        let rules = opts
            .rules
            .iter()
            .filter(|r| !r.left.trim().is_empty() && !r.right.trim().is_empty())
            .map(|r| {
                Ok((
                    mask_regex(&prep(&r.left), ignore_case)?,
                    mask_regex(&prep(&r.right), ignore_case)?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            ignore_case,
            unicode: opts.unicode,
            ignore_extensions: opts.ignore_extensions,
            groups,
            rules,
        })
    }

    /// The pairing key of an entry named `name` on `side`.
    pub fn key(&self, side: Side, name: &str, is_dir: bool) -> String {
        let name = if self.unicode {
            nfc(name)
        } else {
            name.to_owned()
        };
        let fold = |s: &str| {
            if self.ignore_case {
                s.to_lowercase()
            } else {
                s.to_owned()
            }
        };

        for (i, (left, right)) in self.rules.iter().enumerate() {
            let re = if side == Side::Left { left } else { right };
            if let Some(caps) = re.captures(&name) {
                let mut key = format!("{SEP}{i}");
                for m in caps.iter().skip(1) {
                    key.push(SEP);
                    key.push_str(&fold(m.map_or("", |m| m.as_str())));
                }
                return key;
            }
        }

        if !is_dir {
            match split_ext(&name) {
                Some((stem, _)) if self.ignore_extensions => return format!("{}{EXT}", fold(stem)),
                None if self.ignore_extensions => return format!("{}{EXT}", fold(&name)),
                Some((stem, ext)) => {
                    if let Some(group) = self.groups.get(&ext.to_lowercase()) {
                        return format!("{}{EXT}{group}", fold(stem));
                    }
                }
                None => {}
            }
        }
        fold(&name)
    }

    /// Key for an entry whose folded key collided with an earlier sibling's:
    /// its exact name, marked so it can't collide with a folded key.
    pub fn exact_key(name: &str) -> String {
        format!("{SEP}{SEP}{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aligner(opts: AlignOptions, ignore_case: bool) -> Aligner {
        Aligner::new(&opts, ignore_case).unwrap()
    }

    fn pairs(a: &Aligner, left: &str, right: &str) -> bool {
        a.key(Side::Left, left, false) == a.key(Side::Right, right, false)
    }

    #[test]
    fn unicode_forms_align_by_default() {
        let (nfc, nfd) = ("caf\u{e9}.txt", "cafe\u{301}.txt");
        assert!(pairs(&aligner(AlignOptions::default(), false), nfc, nfd));
        let off = AlignOptions {
            unicode: false,
            ..Default::default()
        };
        assert!(!pairs(&aligner(off, false), nfc, nfd));
    }

    #[test]
    fn case_follows_the_option() {
        assert!(!pairs(
            &aligner(AlignOptions::default(), false),
            "Readme.md",
            "README.md"
        ));
        assert!(pairs(
            &aligner(AlignOptions::default(), true),
            "Readme.md",
            "README.md"
        ));
    }

    #[test]
    fn extension_groups_and_ignoring_extensions() {
        let groups = AlignOptions {
            extension_groups: vec!["htm, .html".into(), "jpg jpeg".into()],
            ..Default::default()
        };
        let a = aligner(groups, false);
        assert!(pairs(&a, "index.htm", "index.html"));
        assert!(pairs(&a, "photo.JPEG", "photo.jpg"));
        assert!(!pairs(&a, "index.htm", "index.jpg"));
        assert!(!pairs(&a, "index.htm", "main.html"));

        let a = aligner(
            AlignOptions {
                ignore_extensions: true,
                ..Default::default()
            },
            false,
        );
        assert!(pairs(&a, "report.txt", "report.md"));
        assert!(pairs(&a, "report", "report.md"));
        // Folders keep their names, so a file never pairs with a folder by stem.
        assert_ne!(
            a.key(Side::Left, "report.txt", false),
            a.key(Side::Right, "report", true)
        );
        // Dot files have no extension to drop.
        assert!(!pairs(&a, ".gitignore", ".gitattributes"));
    }

    #[test]
    fn wildcard_rules_pair_on_the_matched_parts() {
        let rules = AlignOptions {
            rules: vec![AlignRule {
                left: "Apples*".into(),
                right: "Bananas*".into(),
            }],
            ..Default::default()
        };
        let a = aligner(rules.clone(), false);
        assert!(pairs(&a, "Apples-2024.csv", "Bananas-2024.csv"));
        assert!(!pairs(&a, "Apples-2024.csv", "Bananas-2025.csv"));
        // Masks apply per side: a right-hand "Apples" file doesn't match the left mask's side.
        assert!(!pairs(&a, "Bananas-1", "Apples-1"));
        // Unmatched names still pair by name.
        assert!(pairs(&a, "other.txt", "other.txt"));
        // Case-insensitive sessions match masks and captures without case.
        let a = aligner(rules, true);
        assert!(pairs(&a, "apples-X", "BANANAS-x"));

        let q = AlignOptions {
            rules: vec![AlignRule {
                left: "v?.log".into(),
                right: "version?.log".into(),
            }],
            ..Default::default()
        };
        assert!(pairs(&aligner(q, false), "v1.log", "version1.log"));
    }

    #[test]
    fn masks_escape_regex_metacharacters() {
        let odd = AlignOptions {
            rules: vec![AlignRule {
                left: "a(b[*".into(),
                right: "c)+*".into(),
            }],
            ..Default::default()
        };
        assert!(pairs(&aligner(odd, false), "a(b[1", "c)+1"));
    }
}
