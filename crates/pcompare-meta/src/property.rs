//! Flat, grouped property lists and their side-by-side comparison.

use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffStatus {
    Same,
    Different,
    LeftOnly,
    RightOnly,
}

impl DiffStatus {
    pub fn of<T: PartialEq>(left: Option<&T>, right: Option<&T>) -> Self {
        match (left, right) {
            (Some(l), Some(r)) if l == r => DiffStatus::Same,
            (Some(_), Some(_)) => DiffStatus::Different,
            (Some(_), None) => DiffStatus::LeftOnly,
            (None, _) => DiffStatus::RightOnly,
        }
    }
}

/// One named value read from one side, e.g. `Version / FileVersion = 1.2.3.4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub group: String,
    pub name: String,
    pub value: String,
}

impl Property {
    pub fn new(
        group: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Property {
            group: group.into(),
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyRow {
    pub group: String,
    pub name: String,
    pub left: Option<String>,
    pub right: Option<String>,
    pub status: DiffStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyStats {
    pub same: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
}

impl PropertyStats {
    pub fn of(rows: &[PropertyRow]) -> Self {
        let mut s = PropertyStats::default();
        for r in rows {
            match r.status {
                DiffStatus::Same => s.same += 1,
                DiffStatus::Different => s.different += 1,
                DiffStatus::LeftOnly => s.left_only += 1,
                DiffStatus::RightOnly => s.right_only += 1,
            }
        }
        s
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyCompareResult {
    pub rows: Vec<PropertyRow>,
    pub stats: PropertyStats,
}

impl PropertyCompareResult {
    pub fn new(left: &[Property], right: &[Property]) -> Self {
        let rows = compare_properties(left, right);
        let stats = PropertyStats::of(&rows);
        PropertyCompareResult { rows, stats }
    }
}

/// Repeated names within a group get a ` (2)`, ` (3)` … suffix so every
/// property has a unique key to align on.
fn uniquify(props: &[Property]) -> Vec<(String, String, &str)> {
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    props
        .iter()
        .map(|p| {
            let n = seen.entry((p.group.clone(), p.name.clone())).or_default();
            *n += 1;
            let name = if *n == 1 {
                p.name.clone()
            } else {
                format!("{} ({n})", p.name)
            };
            (p.group.clone(), name, p.value.as_str())
        })
        .collect()
}

/// Aligns two property lists by `(group, name)`.
///
/// Groups keep their first-seen order (left, then right). Within a group the
/// left order wins; right-only properties are slotted in after the nearest
/// preceding property they share with the left, so related rows stay together.
pub fn compare_properties(left: &[Property], right: &[Property]) -> Vec<PropertyRow> {
    let left = uniquify(left);
    let right = uniquify(right);

    let mut groups: Vec<&str> = Vec::new();
    let mut seen_groups = HashSet::new();
    for (g, _, _) in left.iter().chain(&right) {
        if seen_groups.insert(g.as_str()) {
            groups.push(g);
        }
    }

    let mut rows = Vec::with_capacity(left.len().max(right.len()));
    for group in groups {
        let l: Vec<_> = left.iter().filter(|p| p.0 == group).collect();
        let r: Vec<_> = right.iter().filter(|p| p.0 == group).collect();
        let r_values: HashMap<&str, &str> = r.iter().map(|p| (p.1.as_str(), p.2)).collect();
        let l_names: HashSet<&str> = l.iter().map(|p| p.1.as_str()).collect();

        // Names in display order, built from the left then merged with right.
        let mut order: Vec<&str> = l.iter().map(|p| p.1.as_str()).collect();
        let mut cursor = 0;
        for p in &r {
            match order.iter().position(|n| *n == p.1) {
                Some(i) if l_names.contains(p.1.as_str()) => cursor = i + 1,
                _ => {
                    order.insert(cursor, &p.1);
                    cursor += 1;
                }
            }
        }

        let l_values: HashMap<&str, &str> = l.iter().map(|p| (p.1.as_str(), p.2)).collect();
        for name in order {
            let lv = l_values.get(name).map(|v| v.to_string());
            let rv = r_values.get(name).map(|v| v.to_string());
            rows.push(PropertyRow {
                group: group.to_owned(),
                name: name.to_owned(),
                status: DiffStatus::of(lv.as_ref(), rv.as_ref()),
                left: lv,
                right: rv,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(g: &str, n: &str, v: &str) -> Property {
        Property::new(g, n, v)
    }

    #[test]
    fn aligns_and_classifies() {
        let left = vec![
            p("A", "x", "1"),
            p("A", "y", "2"),
            p("A", "z", "3"),
            p("B", "k", "v"),
        ];
        let right = vec![
            p("A", "x", "1"),
            p("A", "new", "n"),
            p("A", "z", "30"),
            p("C", "q", "w"),
        ];
        let rows = compare_properties(&left, &right);
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (r.group.as_str(), r.name.as_str(), r.status))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("A", "x", DiffStatus::Same),
                ("A", "new", DiffStatus::RightOnly),
                ("A", "y", DiffStatus::LeftOnly),
                ("A", "z", DiffStatus::Different),
                ("B", "k", DiffStatus::LeftOnly),
                ("C", "q", DiffStatus::RightOnly),
            ]
        );
        let stats = PropertyStats::of(&rows);
        assert_eq!(
            (
                stats.same,
                stats.different,
                stats.left_only,
                stats.right_only
            ),
            (1, 1, 2, 2)
        );
    }

    #[test]
    fn duplicate_names_are_numbered() {
        let left = vec![p("Tags", "Artist", "a"), p("Tags", "Artist", "b")];
        let right = vec![p("Tags", "Artist", "a")];
        let rows = compare_properties(&left, &right);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, DiffStatus::Same);
        assert_eq!(rows[1].name, "Artist (2)");
        assert_eq!(rows[1].status, DiffStatus::LeftOnly);
    }

    #[test]
    fn empty_sides() {
        assert!(compare_properties(&[], &[]).is_empty());
        let rows = compare_properties(&[], &[p("G", "n", "v")]);
        assert_eq!(rows[0].status, DiffStatus::RightOnly);
        assert_eq!(rows[0].left, None);
    }
}
