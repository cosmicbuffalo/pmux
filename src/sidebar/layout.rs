//! Pure tmux window-layout math: parsing layout strings, removing or
//! reflowing the sidebar leaf, and distributing widths between panes.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutSplit {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LayoutNode {
    width: u16,
    height: u16,
    x: u16,
    y: u16,
    pane_index: Option<u16>,
    split: Option<LayoutSplit>,
    children: Vec<LayoutNode>,
}

impl LayoutNode {
    fn parse(layout: &str) -> Result<Self> {
        let layout = layout.trim();
        // tmux 3.8 reports `#{window_layout}` as JSON; older versions use the
        // `checksum,WxH,X,Y...` string. `select-layout` accepts the old string
        // on every version, so layouts are always written back in it.
        if layout.starts_with('{') {
            return Self::parse_json(layout);
        }
        let (_, body) = layout
            .split_once(',')
            .ok_or_else(|| anyhow!("outer tmux returned a malformed window layout"))?;
        let mut parser = LayoutParser::new(body);
        let node = parser.parse_node()?;
        if parser.peek().is_some() {
            bail!("outer tmux returned trailing data in its window layout");
        }
        Ok(node)
    }

    /// Parse tmux 3.8's JSON layout (`{"V":2,"L":{...}}`).
    fn parse_json(layout: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(layout)
            .map_err(|err| anyhow!("outer tmux returned a malformed window layout: {err}"))?;
        let root = value
            .get("L")
            .ok_or_else(|| anyhow!("outer tmux returned a window layout without a root"))?;
        Self::from_json_cell(root)
    }

    fn from_json_cell(cell: &serde_json::Value) -> Result<Self> {
        let dimension = |key: &str| -> Result<u16> {
            cell.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| {
                    anyhow!("outer tmux returned a window layout without a valid '{key}'")
                })
        };
        let (width, height, x, y) = (
            dimension("w")?,
            dimension("h")?,
            dimension("x")?,
            dimension("y")?,
        );
        match cell.get("t").and_then(serde_json::Value::as_str) {
            Some("p") => {
                // A floating pane has no place in the tiled layout string pmux
                // writes back, so leave such windows alone.
                if cell.get("z").is_some() {
                    bail!("outer tmux window has a floating pane");
                }
                // "I" is the pane id ("%5"), which the old format's leaf
                // number is; "i" is only the pane's position in the window.
                let pane_index = cell
                    .get("I")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| id.strip_prefix('%'))
                    .and_then(|id| id.parse().ok())
                    .ok_or_else(|| {
                        anyhow!("outer tmux returned a pane layout without a pane id")
                    })?;
                Ok(LayoutNode {
                    width,
                    height,
                    x,
                    y,
                    pane_index: Some(pane_index),
                    split: None,
                    children: Vec::new(),
                })
            }
            Some(kind @ ("h" | "v")) => {
                let children = cell
                    .get("c")
                    .and_then(serde_json::Value::as_array)
                    .filter(|children| !children.is_empty())
                    .ok_or_else(|| anyhow!("outer tmux returned a split layout without panes"))?
                    .iter()
                    .map(Self::from_json_cell)
                    .collect::<Result<Vec<_>>>()?;
                Ok(LayoutNode {
                    width,
                    height,
                    x,
                    y,
                    pane_index: None,
                    split: Some(if kind == "h" {
                        LayoutSplit::Horizontal
                    } else {
                        LayoutSplit::Vertical
                    }),
                    children,
                })
            }
            _ => bail!("outer tmux returned a malformed pane layout"),
        }
    }

    /// Remove the full-height sidebar leaf and reflow the remaining *current*
    /// layout. Opening-width deltas reverse only the sidebar's own impact;
    /// manual resizes made while it was open remain intact.
    fn remove_sidebar_and_reflow(
        mut self,
        sidebar_pane_indices: &HashSet<u16>,
        width_deltas: &HashMap<u16, i32>,
    ) -> Option<Self> {
        if self.split != Some(LayoutSplit::Horizontal) {
            return None;
        }
        let sidebar_side = self
            .children
            .first()
            .and_then(|child| {
                child
                    .pane_index
                    .is_some_and(|index| sidebar_pane_indices.contains(&index))
                    .then_some(SidebarSide::Left)
            })
            .or_else(|| {
                self.children.last().and_then(|child| {
                    child
                        .pane_index
                        .is_some_and(|index| sidebar_pane_indices.contains(&index))
                        .then_some(SidebarSide::Right)
                })
            });
        let before = self.children.len();
        self.children.retain(|child| {
            !child
                .pane_index
                .is_some_and(|index| sidebar_pane_indices.contains(&index))
        });
        if self.children.len() == before || self.children.is_empty() {
            return None;
        }
        if width_deltas.is_empty() {
            self.resize_to(self.width, self.height, self.x, self.y);
        } else {
            self.resize_to_with_width_deltas(
                self.width,
                self.height,
                self.x,
                self.y,
                width_deltas,
                sidebar_side,
            );
        }
        Some(self)
    }

    fn resize_to(&mut self, width: u16, height: u16, x: u16, y: u16) {
        self.width = width;
        self.height = height;
        self.x = x;
        self.y = y;
        match self.split {
            None => {}
            Some(LayoutSplit::Horizontal) => {
                let widths =
                    distribute_dimension(self.children.iter().map(|child| child.width), width);
                let mut child_x = x;
                for (child, child_width) in self.children.iter_mut().zip(widths) {
                    child.resize_to(child_width, height, child_x, y);
                    child_x = child_x.saturating_add(child_width.saturating_add(1));
                }
            }
            Some(LayoutSplit::Vertical) => {
                let heights =
                    distribute_dimension(self.children.iter().map(|child| child.height), height);
                let mut child_y = y;
                for (child, child_height) in self.children.iter_mut().zip(heights) {
                    child.resize_to(width, child_height, x, child_y);
                    child_y = child_y.saturating_add(child_height.saturating_add(1));
                }
            }
        }
    }

    /// Place this node at the given geometry, splitting its space evenly
    /// between its direct children at every level. Leaves in `fixed_width`
    /// keep their current width inside a horizontal split. Returns `None` if
    /// some pane would be squeezed below one cell.
    fn equalize(
        &mut self,
        width: u16,
        height: u16,
        x: u16,
        y: u16,
        fixed_width: &HashSet<u16>,
    ) -> Option<()> {
        self.width = width;
        self.height = height;
        self.x = x;
        self.y = y;
        let is_fixed = |child: &Self| {
            child
                .pane_index
                .is_some_and(|index| fixed_width.contains(&index))
        };
        match self.split {
            None => {}
            Some(LayoutSplit::Horizontal) => {
                let fixed_total = self
                    .children
                    .iter()
                    .filter(|child| is_fixed(child))
                    .map(|child| child.width)
                    .sum::<u16>();
                let flexible_count = self
                    .children
                    .iter()
                    .filter(|child| !is_fixed(child))
                    .count();
                let available = width
                    .checked_sub(self.children.len().saturating_sub(1) as u16)?
                    .checked_sub(fixed_total)?;
                if available < flexible_count as u16 {
                    return None;
                }
                let mut flexible_widths =
                    distribute_amount(&vec![1; flexible_count], available).into_iter();
                let mut child_x = x;
                for child in &mut self.children {
                    let child_width = if is_fixed(child) {
                        child.width
                    } else {
                        flexible_widths.next()?
                    };
                    child.equalize(child_width, height, child_x, y, fixed_width)?;
                    child_x = child_x.saturating_add(child_width.saturating_add(1));
                }
            }
            Some(LayoutSplit::Vertical) => {
                let count = self.children.len();
                let available = height.checked_sub(count.saturating_sub(1) as u16)?;
                if available < count as u16 {
                    return None;
                }
                let heights = distribute_amount(&vec![1; count], available);
                let mut child_y = y;
                for (child, child_height) in self.children.iter_mut().zip(heights) {
                    child.equalize(width, child_height, x, child_y, fixed_width)?;
                    child_y = child_y.saturating_add(child_height.saturating_add(1));
                }
            }
        }
        Some(())
    }

    fn resize_to_with_width_deltas(
        &mut self,
        width: u16,
        height: u16,
        x: u16,
        y: u16,
        width_deltas: &HashMap<u16, i32>,
        sidebar_side: Option<SidebarSide>,
    ) {
        self.width = width;
        self.height = height;
        self.x = x;
        self.y = y;
        match self.split {
            None => {}
            Some(LayoutSplit::Horizontal) => {
                let preferred_widths = self
                    .children
                    .iter()
                    .map(|child| child.preferred_width(width_deltas))
                    .collect::<Vec<_>>();
                let restoration_weights = self
                    .children
                    .iter()
                    .map(|child| child.restoration_weight(width_deltas))
                    .collect::<Vec<_>>();
                let available = width.saturating_sub(self.children.len().saturating_sub(1) as u16);
                let widths = sidebar_side.map_or_else(
                    || fit_dimensions(&preferred_widths, available, &restoration_weights),
                    |side| fit_dimensions_at_sidebar_edge(&preferred_widths, available, side),
                );
                let mut child_x = x;
                for (child, child_width) in self.children.iter_mut().zip(widths) {
                    child.resize_to_with_width_deltas(
                        child_width,
                        height,
                        child_x,
                        y,
                        width_deltas,
                        None,
                    );
                    child_x = child_x.saturating_add(child_width.saturating_add(1));
                }
            }
            Some(LayoutSplit::Vertical) => {
                let mut child_y = y;
                for child in &mut self.children {
                    let child_height = child.height;
                    child.resize_to_with_width_deltas(
                        width,
                        child_height,
                        x,
                        child_y,
                        width_deltas,
                        None,
                    );
                    child_y = child_y.saturating_add(child_height.saturating_add(1));
                }
            }
        }
    }

    fn preferred_width(&self, width_deltas: &HashMap<u16, i32>) -> u16 {
        match self.split {
            None => apply_width_delta(
                self.width,
                self.pane_index
                    .and_then(|index| width_deltas.get(&index).copied())
                    .unwrap_or_default(),
            ),
            Some(LayoutSplit::Horizontal) => self
                .children
                .iter()
                .map(|child| child.preferred_width(width_deltas))
                .sum::<u16>()
                .saturating_add(self.children.len().saturating_sub(1) as u16),
            Some(LayoutSplit::Vertical) => {
                apply_width_delta(self.width, self.average_width_delta(width_deltas))
            }
        }
    }

    fn restoration_weight(&self, width_deltas: &HashMap<u16, i32>) -> u16 {
        match self.split {
            None => self
                .pane_index
                .and_then(|index| width_deltas.get(&index).copied())
                .unwrap_or_default()
                .max(0) as u16,
            Some(LayoutSplit::Horizontal) => self
                .children
                .iter()
                .map(|child| child.restoration_weight(width_deltas))
                .sum(),
            Some(LayoutSplit::Vertical) => self.average_width_delta(width_deltas).max(0) as u16,
        }
    }

    fn average_width_delta(&self, width_deltas: &HashMap<u16, i32>) -> i32 {
        let mut deltas = Vec::new();
        self.collect_width_deltas(width_deltas, &mut deltas);
        if deltas.is_empty() {
            0
        } else {
            deltas.iter().sum::<i32>() / deltas.len() as i32
        }
    }

    fn collect_width_deltas(&self, width_deltas: &HashMap<u16, i32>, into: &mut Vec<i32>) {
        if let Some(index) = self.pane_index {
            into.push(width_deltas.get(&index).copied().unwrap_or_default());
        }
        for child in &self.children {
            child.collect_width_deltas(width_deltas, into);
        }
    }

    fn leaf_widths(&self, into: &mut HashMap<u16, u16>) {
        if let Some(index) = self.pane_index {
            into.insert(index, self.width);
        }
        for child in &self.children {
            child.leaf_widths(into);
        }
    }

    pub(super) fn encode_with_checksum(&self) -> String {
        let body = self.encode();
        format!("{:04x},{body}", layout_checksum(&body))
    }

    fn encode(&self) -> String {
        let mut encoded = format!("{}x{},{},{}", self.width, self.height, self.x, self.y);
        match self.split {
            None => encoded.push_str(&format!(",{}", self.pane_index.unwrap_or_default())),
            Some(LayoutSplit::Horizontal) => {
                encoded.push('{');
                encoded.push_str(
                    &self
                        .children
                        .iter()
                        .map(Self::encode)
                        .collect::<Vec<_>>()
                        .join(","),
                );
                encoded.push('}');
            }
            Some(LayoutSplit::Vertical) => {
                encoded.push('[');
                encoded.push_str(
                    &self
                        .children
                        .iter()
                        .map(Self::encode)
                        .collect::<Vec<_>>()
                        .join(","),
                );
                encoded.push(']');
            }
        }
        encoded
    }
}

struct LayoutParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> LayoutParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
        }
    }

    fn parse_node(&mut self) -> Result<LayoutNode> {
        let width = self.number("width")?;
        self.expect(b'x')?;
        let height = self.number("height")?;
        self.expect(b',')?;
        let x = self.number("x offset")?;
        self.expect(b',')?;
        let y = self.number("y offset")?;
        match self.peek() {
            Some(b',') => {
                self.position += 1;
                Ok(LayoutNode {
                    width,
                    height,
                    x,
                    y,
                    pane_index: Some(self.number("pane index")?),
                    split: None,
                    children: Vec::new(),
                })
            }
            Some(b'{') | Some(b'[') => {
                let split = if self.input[self.position] == b'{' {
                    LayoutSplit::Horizontal
                } else {
                    LayoutSplit::Vertical
                };
                let closing = if split == LayoutSplit::Horizontal {
                    b'}'
                } else {
                    b']'
                };
                self.position += 1;
                let mut children = Vec::new();
                loop {
                    children.push(self.parse_node()?);
                    match self.peek() {
                        Some(b',') => self.position += 1,
                        Some(byte) if byte == closing => {
                            self.position += 1;
                            break;
                        }
                        _ => bail!("outer tmux returned a malformed split layout"),
                    }
                }
                Ok(LayoutNode {
                    width,
                    height,
                    x,
                    y,
                    pane_index: None,
                    split: Some(split),
                    children,
                })
            }
            _ => bail!("outer tmux returned a malformed pane layout"),
        }
    }

    fn number(&mut self, field: &str) -> Result<u16> {
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        if self.position == start {
            bail!("outer tmux layout is missing {field}");
        }
        std::str::from_utf8(&self.input[start..self.position])
            .context("outer tmux returned non-UTF-8 layout data")?
            .parse::<u16>()
            .with_context(|| format!("outer tmux returned an invalid layout {field}"))
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        if self.peek() != Some(expected) {
            bail!("outer tmux returned a malformed window layout");
        }
        self.position += 1;
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}

/// Distribute a resized split proportionally, accounting for tmux's one-cell
/// separator between sibling panes. Largest remainders are assigned from the
/// left/top, producing a stable exact integer layout.
fn distribute_dimension(weights: impl Iterator<Item = u16>, dimension: u16) -> Vec<u16> {
    let weights = weights.collect::<Vec<_>>();
    let separators = weights.len().saturating_sub(1) as u16;
    let available = dimension.saturating_sub(separators);
    distribute_amount(&weights, available)
}

fn distribute_amount(weights: &[u16], amount: u16) -> Vec<u16> {
    let total_weight = weights.iter().map(|&weight| u64::from(weight)).sum::<u64>();
    if total_weight == 0 {
        return weights.to_vec();
    }
    let mut allocations = weights
        .iter()
        .map(|&weight| ((u64::from(weight) * u64::from(amount)) / total_weight) as u16)
        .collect::<Vec<_>>();
    let allocated = allocations
        .iter()
        .map(|&width| u64::from(width))
        .sum::<u64>();
    let remaining = usize::try_from(u64::from(amount).saturating_sub(allocated)).unwrap_or(0);
    let mut order = (0..weights.len()).collect::<Vec<_>>();
    order.sort_by(|&left, &right| {
        let left_remainder = (u64::from(weights[left]) * u64::from(amount)) % total_weight;
        let right_remainder = (u64::from(weights[right]) * u64::from(amount)) % total_weight;
        right_remainder.cmp(&left_remainder).then(left.cmp(&right))
    });
    for index in order.into_iter().take(remaining) {
        allocations[index] = allocations[index].saturating_add(1);
    }
    allocations
}

fn apply_width_delta(width: u16, delta: i32) -> u16 {
    let adjusted = i32::from(width).saturating_add(delta).max(1);
    u16::try_from(adjusted).unwrap_or(u16::MAX)
}

/// Fit manually preserved preferred widths into an enclosing split. Any
/// difference left by a user-resized sidebar is shared in the same direction
/// as the sidebar's original opening adjustment.
fn fit_dimensions(preferred: &[u16], dimension: u16, restoration_weights: &[u16]) -> Vec<u16> {
    let mut fitted = preferred.to_vec();
    let total = fitted.iter().map(|&width| i32::from(width)).sum::<i32>();
    let difference = i32::from(dimension).saturating_sub(total);
    if difference == 0 || fitted.is_empty() {
        return fitted;
    }
    let weights = if restoration_weights.iter().any(|&weight| weight > 0) {
        restoration_weights
    } else {
        preferred
    };
    if difference > 0 {
        let additions = distribute_amount(weights, difference as u16);
        for (width, addition) in fitted.iter_mut().zip(additions) {
            *width = width.saturating_add(addition);
        }
    } else {
        let removals = distribute_amount(weights, (-difference) as u16);
        for (width, removal) in fitted.iter_mut().zip(removals) {
            *width = width.saturating_sub(removal).max(1);
        }
    }
    fitted
}

/// A sidebar resize changes only the work pane immediately next to its edge.
/// On close, give the residual back to that same edge pane instead of
/// distributing it across every work pane and destroying the user's splitter
/// positions.
fn fit_dimensions_at_sidebar_edge(
    preferred: &[u16],
    dimension: u16,
    sidebar_side: SidebarSide,
) -> Vec<u16> {
    let mut fitted = preferred.to_vec();
    let total = fitted.iter().map(|&width| i32::from(width)).sum::<i32>();
    let difference = i32::from(dimension).saturating_sub(total);
    if difference == 0 || fitted.is_empty() {
        return fitted;
    }
    let edge_index = match sidebar_side {
        SidebarSide::Left => 0,
        SidebarSide::Right => fitted.len().saturating_sub(1),
    };
    let adjusted = i32::from(fitted[edge_index]).saturating_add(difference);
    if adjusted < 1 {
        return fit_dimensions(preferred, dimension, preferred);
    }
    fitted[edge_index] = adjusted as u16;
    fitted
}

pub(super) fn opening_width_deltas(
    before_layout: &str,
    after_layout: &str,
    panes_before: &[OuterPane],
    panes_after: &[OuterPane],
) -> Option<String> {
    let mut before = HashMap::new();
    LayoutNode::parse(before_layout)
        .ok()?
        .leaf_widths(&mut before);
    let mut after = HashMap::new();
    LayoutNode::parse(after_layout)
        .ok()?
        .leaf_widths(&mut after);
    let mut deltas = panes_before
        .iter()
        .filter_map(|before_pane| {
            let after_pane = panes_after
                .iter()
                .find(|after_pane| after_pane.id == before_pane.id)?;
            let before_width = *before.get(&before_pane.layout_pane_id)?;
            let after_width = *after.get(&after_pane.layout_pane_id)?;
            let delta = i32::from(before_width) - i32::from(after_width);
            (delta != 0).then_some((after_pane.layout_pane_id, delta))
        })
        .collect::<Vec<_>>();
    deltas.sort_by_key(|(pane_index, _)| *pane_index);
    (!deltas.is_empty()).then(|| {
        deltas
            .into_iter()
            .map(|(pane_index, delta)| format!("{pane_index}:{delta}"))
            .collect::<Vec<_>>()
            .join(",")
    })
}

pub(super) fn parse_width_deltas(encoded: &str) -> Option<HashMap<u16, i32>> {
    let mut deltas = HashMap::new();
    for entry in encoded.split(',') {
        let (pane_index, delta) = entry.split_once(':')?;
        deltas.insert(pane_index.parse().ok()?, delta.parse().ok()?);
    }
    (!deltas.is_empty()).then_some(deltas)
}

fn layout_checksum(layout: &str) -> u16 {
    layout.bytes().fold(0_u16, |checksum, byte| {
        (checksum >> 1)
            .wrapping_add((checksum & 1) << 15)
            .wrapping_add(u16::from(byte))
    })
}

pub(super) fn dynamic_layout_without_sidebars(
    layout: &str,
    sidebars: &[OuterPane],
    width_deltas: Option<&HashMap<u16, i32>>,
) -> Option<LayoutNode> {
    let sidebar_indices = sidebars
        .iter()
        .map(|pane| pane.layout_pane_id)
        .collect::<HashSet<_>>();
    let empty_deltas = HashMap::new();
    let width_deltas = width_deltas.unwrap_or(&empty_deltas);
    LayoutNode::parse(layout)
        .ok()?
        .remove_sidebar_and_reflow(&sidebar_indices, width_deltas)
}

/// Equalize the window layout while keeping its shape: every split gives each
/// of its direct children an equal share of its space, recursively. A marked
/// sidebar keeps its current width and is left out of the distribution.
pub(super) fn equalized_layout(layout: &str, panes: &[OuterPane]) -> Option<LayoutNode> {
    let sidebar_indices = panes
        .iter()
        .filter(|pane| pane.sidebar)
        .map(|pane| pane.layout_pane_id)
        .collect::<HashSet<_>>();
    let mut layout = LayoutNode::parse(layout).ok()?;
    let (width, height, x, y) = (layout.width, layout.height, layout.x, layout.y);
    layout.equalize(width, height, x, y, &sidebar_indices)?;
    Some(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    // tmux 3.8 changed `#{window_layout}` to JSON. Both spellings of the same
    // window (captured from tmux 3.5a and 3.8) must parse identically.
    #[test]
    fn json_layouts_from_tmux_3_8_parse_like_classic_ones() {
        let classic = "c522,120x30,0,0{60x30,0,0,0,59x30,61,0[59x15,61,0,1,59x14,61,16,2]}";
        let json = r#"{"V":2,"L":{"t":"h","w":120,"h":30,"x":0,"y":0,"c":[{"t":"p","w":60,"h":30,"x":0,"y":0,"l":1,"i":0,"I":"%0"},{"t":"v","w":59,"h":30,"x":61,"y":0,"c":[{"t":"p","w":59,"h":15,"x":61,"y":0,"l":0,"i":1,"I":"%1"},{"t":"p","w":59,"h":14,"x":61,"y":16,"a":true,"i":2,"I":"%2"}]}]}}"#;
        let from_json = LayoutNode::parse(json).unwrap();
        assert_eq!(from_json, LayoutNode::parse(classic).unwrap());
        // Written back in the classic form, which every tmux accepts.
        assert_eq!(from_json.encode_with_checksum(), classic);
    }

    #[test]
    fn json_layouts_use_pane_ids_not_window_positions() {
        // Pane %7 sits at position 0: the leaf must carry 7, as the classic
        // format's leaf number would.
        let json = r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0,"a":true,"i":0,"I":"%7"}}"#;
        assert_eq!(LayoutNode::parse(json).unwrap().pane_index, Some(7));
    }

    #[test]
    fn json_layouts_with_a_floating_pane_are_refused() {
        let json = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":40,"h":24,"x":0,"y":0,"i":0,"I":"%0"},{"t":"p","w":39,"h":24,"x":41,"y":0,"i":1,"z":1,"I":"%1"}]}}"#;
        assert!(LayoutNode::parse(json).is_err());
    }

    #[test]
    fn dynamic_reflow_reverses_sidebar_resize_without_a_layout_snapshot() {
        // This is a real tmux layout after resizing a left sidebar from 42 to
        // 51 cells. The ordinary panes have moved in tmux's own layout while
        // the sidebar was open, so simply removing the sidebar is insufficient.
        let layout = "2390,120x30,0,0{51x30,0,0,3,29x30,52,0,0,38x30,82,0,1}";
        let sidebar = OuterPane {
            id: "%3".to_string(),
            sidebar: true,
            layout_pane_id: 3,
        };
        let opening_deltas = HashMap::from([(0, 22), (1, 21)]);

        let reflowed = dynamic_layout_without_sidebars(layout, &[sidebar], Some(&opening_deltas))
            .expect("sidebar leaf is removed");

        assert_eq!(reflowed.encode(), "120x30,0,0{60x30,0,0,0,59x30,61,0,1}");
    }

    fn pane(layout_pane_id: u16, sidebar: bool) -> OuterPane {
        OuterPane {
            id: format!("%{layout_pane_id}"),
            sidebar,
            layout_pane_id,
        }
    }

    #[test]
    fn equalize_keeps_a_left_pane_beside_a_stacked_pair() {
        // One pane on the left, two stacked on the right, all uneven.
        let layout = "0000,121x31,0,0{30x31,0,0,0,90x31,31,0[90x5,31,0,1,90x25,31,6,2]}";

        let equalized = equalized_layout(layout, &[pane(0, false), pane(1, false), pane(2, false)])
            .expect("equalized layout");

        assert_eq!(
            equalized.encode(),
            "121x31,0,0{60x31,0,0,0,60x31,61,0[60x15,61,0,1,60x15,61,16,2]}"
        );
    }

    #[test]
    fn equalize_splits_each_level_between_its_own_children() {
        // Rows: a top row of three columns over a single bottom pane.
        let layout =
            "0000,100x41,0,0[100x10,0,0{10x10,0,0,0,10x10,11,0,1,78x10,22,0,2},100x30,0,11,3]";

        let equalized = equalized_layout(
            layout,
            &[
                pane(0, false),
                pane(1, false),
                pane(2, false),
                pane(3, false),
            ],
        )
        .expect("equalized layout");

        assert_eq!(
            equalized.encode(),
            "100x41,0,0[100x20,0,0{33x20,0,0,0,33x20,34,0,1,32x20,68,0,2},100x20,0,21,3]"
        );
    }

    #[test]
    fn equalize_keeps_a_left_sidebar_at_its_current_width() {
        let layout =
            "0000,120x30,0,0{42x30,0,0,3,20x30,43,0,1,56x30,64,0[56x10,64,0,2,56x19,64,11,4]}";

        let equalized = equalized_layout(
            layout,
            &[
                pane(3, true),
                pane(1, false),
                pane(2, false),
                pane(4, false),
            ],
        )
        .expect("sidebar-preserving equalized layout");

        assert_eq!(
            equalized.encode(),
            "120x30,0,0{42x30,0,0,3,38x30,43,0,1,38x30,82,0[38x15,82,0,2,38x14,82,16,4]}"
        );
    }

    #[test]
    fn equalize_keeps_a_right_sidebar_at_its_current_width() {
        let layout = "0000,120x30,0,0{20x30,0,0,1,56x30,21,0,2,42x30,78,0,3}";

        let equalized = equalized_layout(layout, &[pane(1, false), pane(2, false), pane(3, true)])
            .expect("sidebar-preserving equalized layout");

        assert_eq!(
            equalized.encode(),
            "120x30,0,0{38x30,0,0,1,38x30,39,0,2,42x30,78,0,3}"
        );
    }

    #[test]
    fn equalize_refuses_a_layout_too_small_to_hold_every_pane() {
        let fits = "0000,3x10,0,0{1x10,0,0,0,1x10,2,0,1}";
        assert!(equalized_layout(fits, &[pane(0, false), pane(1, false)]).is_some());

        // The sidebar's two cells leave nothing for the other pane.
        let crowded = "0000,3x10,0,0{2x10,0,0,0,0x10,3,0,1}";
        assert!(equalized_layout(crowded, &[pane(0, true), pane(1, false)]).is_none());
    }
}
