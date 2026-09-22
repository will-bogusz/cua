//! The `query` projection of a walked tree, and the placement facts the
//! structured observation publishes for rows that carry no `element_index`.
//!
//! A walk produces one row per rendered line — node rows (actionable or
//! display-only) and the walker's own notes. Everything here works on those
//! `(depth, text)` rows by position, so the markdown and the structured arrays
//! are cut by the same decision and cannot disagree about what was returned.
//!
//! # Query semantics
//! - A row matches when any literal is a case-insensitive substring of the
//!   row's rendered text. One string is one literal; alternation is an array,
//!   so a literal can contain any character, `|` included.
//! - A match is returned with its ancestor chain, which places it.
//! - A matched row that holds rows beneath it (a container) also returns the
//!   first [`QUERY_EXPANSION_ROWS`] rows of its subtree, in walk order, unless
//!   a deeper match lies inside it — then the deeper match is the specific
//!   answer and the outer one is returned as its ancestor only. Rows the cap
//!   left out are counted per container ([`HiddenUnderMatch`]), never dropped
//!   silently.

/// Rows of a matched container's subtree a query returns with it. A matched
/// container used to answer with its own existence and nothing else (an open
/// popover: `AXPopover ""` with every item hidden), so the consumer re-queried
/// by guessing words. 12 is the bound the consumer measured over its recorded
/// queries: it keeps every container those queries asked for whole while
/// bounding what an accidental match on a window root returns.
pub const QUERY_EXPANSION_ROWS: usize = 12;

/// Rows under one matched container that the expansion cap did not return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenUnderMatch {
    /// Row index of the matched container.
    pub container: usize,
    /// Row index of the last row returned from its subtree.
    pub last_shown: usize,
    /// How many rows of its subtree were not returned.
    pub hidden: usize,
    /// Depth of the note row that counts them: one below the container.
    pub depth: usize,
}

/// What a query kept of one walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryProjection {
    /// Parallel to the walk's rows: whether the projection returns that row.
    pub kept: Vec<bool>,
    /// Rows a literal matched (ancestors and expansion rows not counted).
    pub matched: usize,
    pub hidden: Vec<HiddenUnderMatch>,
}

/// Project `rows` (`(depth, rendered text)`, walk order) onto `literals`.
pub fn project(rows: &[(usize, String)], literals: &[String]) -> QueryProjection {
    let needles: Vec<String> = literals.iter().map(|l| l.to_lowercase()).collect();
    let mut kept = vec![false; rows.len()];
    // Where each row's own subtree ends (exclusive), from the same stack that
    // places its ancestors: a row is a container exactly when the next row is
    // deeper.
    let mut ends = vec![rows.len(); rows.len()];
    let mut open: Vec<usize> = Vec::new();
    let mut matches: Vec<usize> = Vec::new();
    for (index, (depth, text)) in rows.iter().enumerate() {
        while open.last().is_some_and(|&top| rows[top].0 >= *depth) {
            ends[open.pop().unwrap()] = index;
        }
        let haystack = text.to_lowercase();
        if needles.iter().any(|needle| haystack.contains(needle.as_str())) {
            matches.push(index);
            kept[index] = true;
            for &ancestor in &open {
                kept[ancestor] = true;
            }
        }
        open.push(index);
    }

    let mut hidden = Vec::new();
    for (order, &matched) in matches.iter().enumerate() {
        let end = ends[matched];
        if matches.get(order + 1).is_some_and(|&next| next < end) {
            continue;
        }
        let last = end.min(matched + 1 + QUERY_EXPANSION_ROWS);
        for row in kept.iter_mut().take(last).skip(matched + 1) {
            *row = true;
        }
        if end > last {
            hidden.push(HiddenUnderMatch {
                container: matched,
                last_shown: last - 1,
                hidden: end - last,
                depth: rows[matched].0 + 1,
            });
        }
    }
    QueryProjection {
        kept,
        matched: matches.len(),
        hidden,
    }
}

/// The rows a projection returns, with one note row after each capped
/// container's last returned row, one level below the container.
pub fn projected_rows(
    rows: &[(usize, String)],
    projection: &QueryProjection,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if !projection.kept[index] {
            continue;
        }
        out.push(row.clone());
        for capped in projection.hidden.iter().filter(|h| h.last_shown == index) {
            out.push((
                capped.depth,
                format!(
                    "- {} more row(s) under this one were not returned by the query",
                    capped.hidden
                ),
            ));
        }
    }
    out
}

/// Places a row that has no `element_index` among the rows that do: the
/// `element_index` of the actionable row returned immediately before it in
/// walk order, `None` when no returned actionable row precedes it.
pub struct Placer {
    /// `(row, element_index)` of every returned actionable row, in row order.
    anchors: Vec<(usize, usize)>,
}

impl Placer {
    /// `actionable` yields `(row, element_index)` in walk order; `returned`
    /// says whether the projection returned a row.
    pub fn new(
        actionable: impl IntoIterator<Item = (usize, usize)>,
        returned: impl Fn(usize) -> bool,
    ) -> Self {
        Self {
            anchors: actionable
                .into_iter()
                .filter(|&(row, _)| returned(row))
                .collect(),
        }
    }

    /// The anchor of a row printed at `row`.
    pub fn at(&self, row: usize) -> Option<usize> {
        self.before(row)
    }

    /// The anchor of a note printed immediately after `row`.
    pub fn after(&self, row: usize) -> Option<usize> {
        self.before(row + 1)
    }

    fn before(&self, row: usize) -> Option<usize> {
        let count = self.anchors.partition_point(|&(anchor, _)| anchor < row);
        count.checked_sub(1).map(|position| self.anchors[position].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(spec: &[(usize, &str)]) -> Vec<(usize, String)> {
        spec.iter().map(|&(d, t)| (d, t.to_owned())).collect()
    }

    fn kept_texts(rows: &[(usize, String)], projection: &QueryProjection) -> Vec<String> {
        projected_rows(rows, projection)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    fn literals(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn any_literal_matches_and_a_literal_may_contain_a_pipe() {
        let tree = rows(&[
            (0, "- [0] AXWindow \"Chrome\""),
            (1, "- [1] AXRadioButton \"Order Status | Peptaura\""),
            (1, "- [2] AXButton \"Reload\""),
            (1, "- [3] AXButton \"Back\""),
        ]);
        let piped = project(&tree, &literals(&["status | pep"]));
        assert_eq!(
            kept_texts(&tree, &piped),
            ["- [0] AXWindow \"Chrome\"", "- [1] AXRadioButton \"Order Status | Peptaura\""]
        );
        let either = project(&tree, &literals(&["RELOAD", "back"]));
        assert_eq!(either.matched, 2);
        assert_eq!(
            kept_texts(&tree, &either),
            ["- [0] AXWindow \"Chrome\"", "- [2] AXButton \"Reload\"", "- [3] AXButton \"Back\""]
        );
    }

    #[test]
    fn a_matched_container_returns_its_first_rows_and_counts_the_rest() {
        let mut spec = vec![(0, "- [0] AXWindow \"W\"".to_owned())];
        spec.push((1, "- [1] AXPopover \"Colors\"".to_owned()));
        for n in 0..15 {
            spec.push((2, format!("- [{}] AXMenuItem \"item {n}\"", n + 2)));
        }
        spec.push((1, "- [17] AXButton \"Done\"".to_owned()));
        let projection = project(&spec, &literals(&["colors"]));
        let out = projected_rows(&spec, &projection);
        // Window, popover, 12 items, then the note one level below the popover.
        assert_eq!(out.len(), 2 + QUERY_EXPANSION_ROWS + 1);
        assert_eq!(out[2 + QUERY_EXPANSION_ROWS - 1].1, "- [13] AXMenuItem \"item 11\"");
        assert_eq!(
            out.last().unwrap(),
            &(2, "- 3 more row(s) under this one were not returned by the query".to_owned())
        );
        assert_eq!(
            projection.hidden,
            [HiddenUnderMatch { container: 1, last_shown: 13, hidden: 3, depth: 2 }]
        );
        assert!(!out.iter().any(|(_, t)| t.contains("Done")), "a sibling is not the container's");
    }

    #[test]
    fn a_container_holding_a_deeper_match_is_returned_as_its_ancestor_only() {
        let tree = rows(&[
            (0, "- [0] AXWindow \"Mail\""),
            (1, "- [1] AXOutline \"Mailboxes\""),
            (2, "- [2] AXRow \"Inbox\""),
            (2, "- [3] AXRow \"Mailboxes archive\""),
            (2, "- [4] AXRow \"Sent\""),
        ]);
        let projection = project(&tree, &literals(&["mailboxes"]));
        assert_eq!(
            kept_texts(&tree, &projection),
            [
                "- [0] AXWindow \"Mail\"",
                "- [1] AXOutline \"Mailboxes\"",
                "- [3] AXRow \"Mailboxes archive\"",
            ]
        );
        assert!(projection.hidden.is_empty());
    }

    #[test]
    fn a_short_container_is_returned_whole_without_a_note() {
        let tree = rows(&[
            (0, "- [0] AXGroup \"Sidebar\""),
            (1, "- AXStaticText = \"Favorites\""),
            (1, "- [1] AXRow \"Desktop\""),
            (0, "- [2] AXButton \"Other\""),
        ]);
        let projection = project(&tree, &literals(&["sidebar"]));
        assert_eq!(projected_rows(&tree, &projection).len(), 3);
        assert!(projection.hidden.is_empty());
    }

    #[test]
    fn nothing_matched_returns_no_rows() {
        let tree = rows(&[(0, "- [0] AXWindow \"W\"")]);
        let projection = project(&tree, &literals(&["absent"]));
        assert_eq!(projection.matched, 0);
        assert!(projected_rows(&tree, &projection).is_empty());
    }

    #[test]
    fn placement_names_the_returned_actionable_row_before_it() {
        // rows: 0 [0] window, 1 text, 2 [1] button (not returned), 3 text
        let placer = Placer::new([(0, 0), (2, 1)], |row| row != 2);
        assert_eq!(placer.at(1), Some(0));
        assert_eq!(placer.at(3), Some(0), "an unreturned row is no anchor");
        assert_eq!(placer.at(0), None);
        assert_eq!(placer.after(0), Some(0));
        let all = Placer::new([(0, 0), (2, 1)], |_| true);
        assert_eq!(all.at(3), Some(1));
    }
}
