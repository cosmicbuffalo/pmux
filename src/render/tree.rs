//! The lineage tree in front of process rows.
//!
//! A process spawned by another listed process is drawn under it, indented
//! [`TREE_STEP`] columns per generation. A spine drops from the parent's
//! bottom rule (`└─┬`) at the column of its children's left edges, and runs
//! down through them: each child's top corner becomes a tee, and so does the
//! bottom corner of every child but the last:
//!
//! ```text
//! ┌ ● lead ──────────── →
//! │   claude ~/code   pane 2
//! └─┬ working · 2m ago ────
//!   │
//!   ├ ● helper ─────── ◯
//!   ╎   claude ~/code
//!   ├── done · 1m ago ─ ─
//!   │
//!   ├ ● tests ──────── ◯
//!   ╎   process ~/code
//!   └── running · 3m ago ─
//! ```
//!
//! Rows without a box (outline style `none`, the herdr preset) have no left
//! edge to run through, so their spine drops from under the parent's state
//! dot and turns into each child's dot:
//!
//! ```text
//!   ● lead              pane 2
//!   │ claude working · 2m ago
//!   │
//!   ├─● helper
//!   │   claude done · 1m ago
//!   │
//!   └─● tests
//!       process running · 3m ago
//! ```

use super::*;

/// Columns each generation is indented by.
pub const TREE_STEP: usize = 2;

/// Where a process row sits in the lineage tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowTree {
    /// Generations below the row's root: 0 for a root.
    pub depth: usize,
    /// For each ancestor below the root, oldest first, whether a later
    /// sibling of it follows, so the spine through its column carries on.
    pub guides: Vec<bool>,
    /// Whether this is its parent's last child listed.
    pub last: bool,
    /// Whether its own children follow it, nested.
    pub has_children: bool,
}

impl RowTree {
    /// The columns the tree takes in front of the row.
    pub fn width(&self) -> usize {
        self.depth * TREE_STEP
    }
}

/// The tree's glyphs, matched to the outline's: a vertical, the tee and the
/// elbow into a child's top corner, and what a parent's bottom corner
/// becomes. `none`, and outlines of custom glyphs, draw thin lines.
struct TreeGlyphs {
    vertical: &'static str,
    tee: &'static str,
    elbow: &'static str,
    /// `None` when the parent's bottom corner is not a line to branch from.
    junction: Option<&'static str>,
    /// Where the spine leaves a parent's bottom rule, for boxed rows.
    down: &'static str,
}

impl TreeGlyphs {
    fn for_outline(outline: &RowOutline) -> Self {
        match outline.bottom.as_str() {
            "┗" => Self {
                vertical: "┃",
                tee: "┣━",
                elbow: "┗━",
                junction: Some("┣"),
                down: "┳",
            },
            "+" => Self {
                vertical: "|",
                tee: "+-",
                elbow: "`-",
                junction: Some("+"),
                down: "+",
            },
            corner => Self {
                vertical: "│",
                tee: "├─",
                elbow: if corner == "╰" { "╰─" } else { "└─" },
                junction: matches!(corner, "└" | "╰").then_some("├"),
                down: "┬",
            },
        }
    }
}

/// The tree's style: dim like an unfocused outline, and clear of the
/// selection highlight, which belongs to the row itself.
fn tree_style() -> Style {
    Style::default().fg(Color::DarkGray).bg(Color::Reset)
}

/// The ancestors' spines in front of a row: a vertical where an ancestor's
/// later sibling follows, else blank.
fn guide_text(guides: &[bool], glyphs: &TreeGlyphs) -> String {
    guides
        .iter()
        .map(|continues| {
            if *continues {
                format!("{} ", glyphs.vertical)
            } else {
                " ".repeat(TREE_STEP)
            }
        })
        .collect()
}

/// `lines`, one process's row and any open drawer drawn `tree.width()`
/// columns narrower, placed in the tree: each line gains the spines in
/// front of it, the first turns into the row's top corner, and when
/// children follow, the last line's bottom corner branches down to them.
pub fn tree_block(
    lines: Vec<Line<'static>>,
    tree: &RowTree,
    outline: &RowOutline,
) -> Vec<Line<'static>> {
    let glyphs = TreeGlyphs::for_outline(outline);
    if let Some(junction) = glyphs.junction
        && !outline.top.trim().is_empty()
    {
        return boxed_tree_block(lines, tree, outline, &glyphs, junction);
    }
    unboxed_tree_block(lines, tree, &glyphs)
}

/// [`tree_block`] for rows without a box. Their state dot sits
/// [`TREE_STEP`] columns in, behind blank gutter columns; a child's are
/// replaced by its turn, so its dot lands where its parent's spine turns.
fn unboxed_tree_block(
    mut lines: Vec<Line<'static>>,
    tree: &RowTree,
    glyphs: &TreeGlyphs,
) -> Vec<Line<'static>> {
    // The spine to the children drops from under this row's dot.
    if tree.has_children {
        for line in lines.iter_mut().skip(1) {
            replace_blank_at(line, TREE_STEP, glyphs.vertical);
        }
    }
    if tree.depth == 0 {
        return lines;
    }
    let lead = format!(
        "{}{}",
        " ".repeat(TREE_STEP),
        guide_text(&tree.guides, glyphs)
    );
    for (index, line) in lines.iter_mut().enumerate() {
        let own = match (index, tree.last) {
            (0, false) => glyphs.tee.to_string(),
            (0, true) => glyphs.elbow.to_string(),
            (_, false) => format!("{} ", glyphs.vertical),
            (_, true) => " ".repeat(TREE_STEP),
        };
        // A gutter that is not blank (a custom side glyph) stays, after
        // the tree.
        drop_blank_columns(line, TREE_STEP);
        line.spans
            .insert(0, Span::styled(format!("{lead}{own}"), tree_style()));
    }
    lines
}

/// Remove `line`'s first `count` columns when they are all blank.
fn drop_blank_columns(line: &mut Line<'static>, count: usize) {
    let leading: String = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars())
        .take(count)
        .collect();
    if leading.chars().count() < count || !leading.chars().all(|ch| ch == ' ') {
        return;
    }
    let mut left = count;
    for span in &mut line.spans {
        if left == 0 {
            break;
        }
        let chars = span.content.chars().count();
        let take = chars.min(left);
        span.content = span.content.chars().skip(take).collect::<String>().into();
        left -= take;
    }
    line.spans.retain(|span| !span.content.is_empty());
}

/// Put `glyph` at column `col` of `line` when that column is a blank space,
/// dim like the tree but on the row's background, so a selected row's
/// highlight runs unbroken behind it.
fn replace_blank_at(line: &mut Line<'static>, col: usize, glyph: &str) {
    let mut x = 0;
    for index in 0..line.spans.len() {
        let width = text_width(&line.spans[index].content);
        if col < x + width {
            let content = line.spans[index].content.to_string();
            let offset = col - x;
            let mut chars = content.chars();
            let before: String = chars.by_ref().take(offset).collect();
            if before.chars().count() != offset || chars.next() != Some(' ') {
                return;
            }
            let after: String = chars.collect();
            let style = line.spans[index].style;
            line.spans.splice(
                index..=index,
                [
                    Span::styled(before, style),
                    Span::styled(glyph.to_string(), style.fg(Color::DarkGray)),
                    Span::styled(after, style),
                ],
            );
            line.spans.retain(|span| !span.content.is_empty());
            return;
        }
        x += width;
    }
}

/// [`tree_block`] for boxed rows: the spine to a row's children leaves its
/// bottom rule at their left edge, and runs down through their edges.
fn boxed_tree_block(
    mut lines: Vec<Line<'static>>,
    tree: &RowTree,
    outline: &RowOutline,
    glyphs: &TreeGlyphs,
    junction: &'static str,
) -> Vec<Line<'static>> {
    if tree.depth > 0 {
        // The spine arrives from above into the top corner.
        if let Some(first) = lines.first_mut() {
            replace_leading(first, &outline.top, junction);
        }
        // It carries on through the bottom corner to a later sibling.
        if !tree.last
            && let Some(last) = lines.last_mut()
        {
            replace_leading(last, &outline.bottom, junction);
        }
    }
    if tree.has_children
        && let Some(last) = lines.last_mut()
    {
        replace_rule_at(last, TREE_STEP, glyphs.down);
    }
    if tree.depth > 0 {
        let prefix = format!(
            "{}{}",
            " ".repeat(TREE_STEP),
            guide_text(&tree.guides, glyphs)
        );
        for line in &mut lines {
            line.spans
                .insert(0, Span::styled(prefix.clone(), tree_style()));
        }
    }
    lines
}

/// Swap `line`'s leading `corner` for `glyph`, when it starts with it.
fn replace_leading(line: &mut Line<'static>, corner: &str, glyph: &str) {
    if let Some(first) = line.spans.first_mut()
        && let Some(rest) = first.content.strip_prefix(corner)
    {
        first.content = format!("{glyph}{rest}").into();
    }
}

/// Put `glyph` in place of the rule character at column `col` of `line`,
/// keeping its span's style. Text is never overwritten.
fn replace_rule_at(line: &mut Line<'static>, col: usize, glyph: &str) {
    let mut x = 0;
    for span in &mut line.spans {
        let width = text_width(&span.content);
        if col < x + width {
            let mut out = String::new();
            let mut cx = x;
            let mut replaced = false;
            for ch in span.content.chars() {
                if cx == col && matches!(ch, '─' | '━' | '-') {
                    out.push_str(glyph);
                    replaced = true;
                } else {
                    out.push(ch);
                }
                cx += text_width(ch.encode_utf8(&mut [0; 4]));
            }
            if replaced {
                span.content = out.into();
            }
            return;
        }
        x += width;
    }
}

/// The blank line between two rows, carrying the spines down to `next`, the
/// row after it, when that row is a child.
pub fn tree_spacer(next: Option<&RowTree>, outline: &RowOutline) -> Line<'static> {
    let Some(next) = next.filter(|next| next.depth > 0) else {
        return Line::default();
    };
    let glyphs = TreeGlyphs::for_outline(outline);
    // Boxed rows drop the spine from their bottom rule, unboxed ones from
    // under their dot: both [`TREE_STEP`] columns in.
    let lead = " ".repeat(TREE_STEP);
    Line::from(Span::styled(
        format!(
            "{lead}{}{}",
            guide_text(&next.guides, &glyphs),
            glyphs.vertical
        ),
        tree_style(),
    ))
}
