// F6b (monitor/board.rs) is the first caller; until then only the tests below use this module.
#![allow(dead_code)]

use crate::compositor::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PanelId(pub &'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VariantKind {
    Full,
    Compact,
    Mini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Variant {
    pub kind: VariantKind,
    pub min: (usize, usize),
    pub pref: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PanelSpec {
    pub id: PanelId,
    pub priority: u8,
    pub variants: &'static [Variant],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Size {
    Fixed(usize),
    Min(usize),
    Weight(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Row(Vec<(Size, Node)>),
    Col(Vec<(Size, Node)>),
    Panel(PanelId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placed {
    pub id: PanelId,
    pub rect: Rect,
    pub variant: VariantKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Solution {
    pub placed: Vec<Placed>,
    pub dropped: Vec<PanelId>,
    pub overflow_rows: usize,
}

impl Solution {
    pub(crate) fn hidden(&self) -> usize {
        self.dropped.len()
    }
}

#[derive(Debug)]
enum Prepared {
    Row(Vec<(Size, Prepared)>),
    Col(Vec<(Size, Prepared)>),
    Panel(usize),
}

fn prepare(
    node: &Node,
    specs: &[PanelSpec],
    absent: &[PanelId],
    ordered: &mut Vec<usize>,
) -> Option<Prepared> {
    match node {
        Node::Panel(id) => {
            if absent.contains(id) {
                return None;
            }
            let index = specs.iter().position(|spec| spec.id == *id)?;
            if specs[index].variants.is_empty() {
                return None;
            }
            ordered.push(index);
            Some(Prepared::Panel(index))
        }
        Node::Row(children) => {
            let retained = children
                .iter()
                .filter_map(|(size, child)| {
                    prepare(child, specs, absent, ordered).map(|child| (*size, child))
                })
                .collect::<Vec<_>>();
            (!retained.is_empty()).then_some(Prepared::Row(retained))
        }
        Node::Col(children) => {
            let retained = children
                .iter()
                .filter_map(|(size, child)| {
                    prepare(child, specs, absent, ordered).map(|child| (*size, child))
                })
                .collect::<Vec<_>>();
            (!retained.is_empty()).then_some(Prepared::Col(retained))
        }
    }
}

fn extent(
    node: &Prepared,
    specs: &[PanelSpec],
    ranks: &[usize],
    active: &[bool],
) -> Option<(usize, usize)> {
    match node {
        Prepared::Panel(index) => active[*index].then(|| specs[*index].variants[ranks[*index]].min),
        Prepared::Row(children) => {
            let mut cols = 0usize;
            let mut rows = 0usize;
            let mut any = false;
            for (size, child) in children {
                if let Some((child_cols, child_rows)) = extent(child, specs, ranks, active) {
                    any = true;
                    cols = cols.saturating_add(axis_min(*size, child_cols));
                    rows = rows.max(child_rows);
                }
            }
            any.then_some((cols, rows))
        }
        Prepared::Col(children) => {
            let mut cols = 0usize;
            let mut rows = 0usize;
            let mut any = false;
            for (size, child) in children {
                if let Some((child_cols, child_rows)) = extent(child, specs, ranks, active) {
                    any = true;
                    cols = cols.max(child_cols);
                    rows = rows.saturating_add(axis_min(*size, child_rows));
                }
            }
            any.then_some((cols, rows))
        }
    }
}

fn axis_min(size: Size, node_min: usize) -> usize {
    match size {
        Size::Fixed(value) | Size::Min(value) => value.max(node_min),
        Size::Weight(_) => node_min,
    }
}

fn preferred(
    node: &Prepared,
    specs: &[PanelSpec],
    ranks: &[usize],
    active: &[bool],
) -> Option<(usize, usize)> {
    match node {
        Prepared::Panel(index) => {
            active[*index].then(|| specs[*index].variants[ranks[*index]].pref)
        }
        Prepared::Row(children) => {
            let mut cols = 0usize;
            let mut rows = 0usize;
            let mut any = false;
            for (size, child) in children {
                if let Some((child_cols, child_rows)) = preferred(child, specs, ranks, active) {
                    any = true;
                    cols = cols.saturating_add(axis_min(*size, child_cols));
                    rows = rows.max(child_rows);
                }
            }
            any.then_some((cols, rows))
        }
        Prepared::Col(children) => {
            let mut cols = 0usize;
            let mut rows = 0usize;
            let mut any = false;
            for (size, child) in children {
                if let Some((child_cols, child_rows)) = preferred(child, specs, ranks, active) {
                    any = true;
                    cols = cols.max(child_cols);
                    rows = rows.saturating_add(axis_min(*size, child_rows));
                }
            }
            any.then_some((cols, rows))
        }
    }
}

fn panel_index(
    node: &Prepared,
    id: PanelId,
    specs: &[PanelSpec],
    active: &[bool],
) -> Option<usize> {
    match node {
        Prepared::Panel(index) => (active[*index] && specs[*index].id == id).then_some(*index),
        Prepared::Row(children) | Prepared::Col(children) => children
            .iter()
            .find_map(|(_, child)| panel_index(child, id, specs, active)),
    }
}

/// Solve a monitor tree. Variant selection is independent of the area's overflow axis,
/// making every smaller area a prefix of the same downgrade chain.
pub(crate) fn solve(
    tree: &Node,
    specs: &[PanelSpec],
    area: Rect,
    zoom: Option<PanelId>,
    absent: &[PanelId],
) -> Solution {
    let mut order = Vec::new();
    let Some(root) = prepare(tree, specs, absent, &mut order) else {
        return Solution {
            placed: Vec::new(),
            dropped: Vec::new(),
            overflow_rows: 0,
        };
    };
    let mut ranks = vec![0usize; specs.len()];
    let mut active = vec![false; specs.len()];
    for index in &order {
        active[*index] = true;
    }

    if let Some(id) = zoom
        && let Some(index) = panel_index(&root, id, specs, &active)
    {
        let variants = specs[index].variants;
        let rank = variants
            .iter()
            .position(|variant| variant.min.0 <= area.cols && variant.min.1 <= area.rows)
            .unwrap_or(variants.len() - 1);
        let variant = variants[rank];
        return Solution {
            placed: vec![Placed {
                id,
                rect: area,
                variant: variant.kind,
            }],
            dropped: Vec::new(),
            overflow_rows: variant.min.1.saturating_sub(area.rows),
        };
    }

    let mut dropped = Vec::new();
    loop {
        let fits = extent(&root, specs, &ranks, &active)
            .is_none_or(|(cols, rows)| cols <= area.cols && rows <= area.rows);
        if fits {
            break;
        }
        let mut pick: Option<usize> = None;
        for index in &order {
            if !active[*index]
                || (specs[*index].priority == 0
                    && ranks[*index] + 1 == specs[*index].variants.len())
            {
                continue;
            }
            if pick.is_none_or(|current| {
                (specs[*index].priority, ranks[*index]) >= (specs[current].priority, ranks[current])
            }) {
                pick = Some(*index);
            }
        }
        let Some(index) = pick else { break };
        if ranks[index] + 1 < specs[index].variants.len() {
            ranks[index] += 1;
        } else {
            active[index] = false;
            dropped.push(specs[index].id);
        }
    }

    let remains_overflow = extent(&root, specs, &ranks, &active)
        .is_some_and(|(cols, rows)| cols > area.cols || rows > area.rows);
    if remains_overflow {
        let mut placed = Vec::new();
        let mut y = area.y;
        let mut total_rows = 0usize;
        for index in order.iter().filter(|index| active[**index]) {
            let variant = *specs[*index].variants.last().expect("nonempty variants");
            placed.push(Placed {
                id: specs[*index].id,
                rect: Rect {
                    x: area.x,
                    y,
                    cols: area.cols,
                    rows: variant.min.1,
                },
                variant: variant.kind,
            });
            y = y.saturating_add(variant.min.1);
            total_rows = total_rows.saturating_add(variant.min.1);
        }
        return Solution {
            placed,
            dropped,
            overflow_rows: total_rows.saturating_sub(area.rows),
        };
    }

    let mut placed = Vec::new();
    if let Some((cols, rows)) = extent(&root, specs, &ranks, &active) {
        distribute(
            &root,
            specs,
            &ranks,
            &active,
            Rect {
                x: area.x,
                y: area.y,
                cols: area.cols.max(cols),
                rows: area.rows.max(rows),
            },
            &mut placed,
        );
    }
    Solution {
        placed,
        dropped,
        overflow_rows: 0,
    }
}

fn distribute(
    node: &Prepared,
    specs: &[PanelSpec],
    ranks: &[usize],
    active: &[bool],
    rect: Rect,
    placed: &mut Vec<Placed>,
) {
    match node {
        Prepared::Panel(index) if active[*index] => placed.push(Placed {
            id: specs[*index].id,
            rect,
            variant: specs[*index].variants[ranks[*index]].kind,
        }),
        Prepared::Panel(_) => {}
        Prepared::Row(children) => {
            distribute_children(children, true, specs, ranks, active, rect, placed);
        }
        Prepared::Col(children) => {
            distribute_children(children, false, specs, ranks, active, rect, placed);
        }
    }
}

fn distribute_children(
    children: &[(Size, Prepared)],
    row: bool,
    specs: &[PanelSpec],
    ranks: &[usize],
    active: &[bool],
    rect: Rect,
    placed: &mut Vec<Placed>,
) {
    let retained = children
        .iter()
        .filter_map(|(size, child)| {
            extent(child, specs, ranks, active).map(|minimum| {
                let pref = preferred(child, specs, ranks, active).unwrap_or(minimum);
                let axis_min = if row { minimum.0 } else { minimum.1 };
                let pref_axis = if row { pref.0 } else { pref.1 };
                (
                    *size,
                    child,
                    axis_min.max(match size {
                        Size::Fixed(value) | Size::Min(value) => *value,
                        Size::Weight(_) => 0,
                    }),
                    pref_axis.max(axis_min),
                )
            })
        })
        .collect::<Vec<_>>();
    if retained.is_empty() {
        return;
    }
    let total_min = retained.iter().map(|(_, _, min, _)| *min).sum::<usize>();
    let available = if row { rect.cols } else { rect.rows };
    let surplus = available.saturating_sub(total_min);
    let has_weight = retained
        .iter()
        .any(|(size, _, _, _)| matches!(size, Size::Weight(weight) if *weight > 0));
    let mut allocations = retained
        .iter()
        .map(|(_, _, min, _)| *min)
        .collect::<Vec<_>>();
    if has_weight {
        let total_weight = retained
            .iter()
            .map(|(size, _, _, _)| match size {
                Size::Weight(weight) => usize::from(*weight),
                Size::Fixed(_) | Size::Min(_) => 0,
            })
            .sum::<usize>();
        let shares = retained
            .iter()
            .map(|(size, _, _, _)| match size {
                Size::Weight(weight) => surplus.saturating_mul(usize::from(*weight)),
                Size::Fixed(_) | Size::Min(_) => 0,
            })
            .collect::<Vec<_>>();
        let floors = shares
            .iter()
            .map(|share| share / total_weight)
            .collect::<Vec<_>>();
        let mut assigned = 0usize;
        let remainders = shares
            .iter()
            .map(|share| share % total_weight)
            .collect::<Vec<_>>();
        for (allocation, floor) in allocations.iter_mut().zip(&floors) {
            *allocation = allocation.saturating_add(*floor);
            assigned = assigned.saturating_add(*floor);
        }
        let leftovers = surplus.saturating_sub(assigned);
        let mut remainder_order = (0..retained.len())
            .filter(|index| matches!(retained[*index].0, Size::Weight(weight) if weight > 0))
            .collect::<Vec<_>>();
        remainder_order.sort_by_key(|index| (std::cmp::Reverse(remainders[*index]), *index));
        for index in remainder_order.into_iter().take(leftovers) {
            allocations[index] = allocations[index].saturating_add(1);
        }
    } else {
        let mut remainder = surplus;
        for (allocation, (_, _, _, pref)) in allocations.iter_mut().zip(&retained) {
            let growth = remainder.min(pref.saturating_sub(*allocation));
            *allocation = allocation.saturating_add(growth);
            remainder -= growth;
        }
        if let Some(last) = allocations.last_mut() {
            *last = last.saturating_add(remainder);
        }
    }

    let mut offset = 0usize;
    for ((_, child, _, _), size) in retained.iter().zip(allocations) {
        let child_rect = if row {
            Rect {
                x: rect.x.saturating_add(offset),
                y: rect.y,
                cols: size,
                rows: rect.rows,
            }
        } else {
            Rect {
                x: rect.x,
                y: rect.y.saturating_add(offset),
                cols: rect.cols,
                rows: size,
            }
        };
        distribute(child, specs, ranks, active, child_rect, placed);
        offset = offset.saturating_add(size);
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, PanelId, PanelSpec, Size, Solution, Variant, VariantKind, solve};
    use crate::compositor::Rect;

    const CPU: PanelId = PanelId("cpu");
    const MEM: PanelId = PanelId("mem");
    const NET: PanelId = PanelId("net");
    const DISK: PanelId = PanelId("disk");
    const PROCS: PanelId = PanelId("procs");
    const CHIPS: PanelId = PanelId("chips");
    const PSI: PanelId = PanelId("psi");
    const CPU_V: &[Variant] = &[
        Variant {
            kind: VariantKind::Full,
            min: (24, 8),
            pref: (50, 16),
        },
        Variant {
            kind: VariantKind::Compact,
            min: (18, 5),
            pref: (24, 8),
        },
        Variant {
            kind: VariantKind::Mini,
            min: (12, 3),
            pref: (18, 5),
        },
    ];
    const OTHER_V: &[Variant] = &[
        Variant {
            kind: VariantKind::Full,
            min: (22, 7),
            pref: (40, 14),
        },
        Variant {
            kind: VariantKind::Compact,
            min: (16, 4),
            pref: (22, 7),
        },
        Variant {
            kind: VariantKind::Mini,
            min: (10, 2),
            pref: (16, 4),
        },
    ];
    const SPECS: &[PanelSpec] = &[
        PanelSpec {
            id: CPU,
            priority: 0,
            variants: CPU_V,
        },
        PanelSpec {
            id: MEM,
            priority: 2,
            variants: OTHER_V,
        },
        PanelSpec {
            id: NET,
            priority: 3,
            variants: OTHER_V,
        },
        PanelSpec {
            id: DISK,
            priority: 3,
            variants: OTHER_V,
        },
        PanelSpec {
            id: PROCS,
            priority: 1,
            variants: OTHER_V,
        },
        PanelSpec {
            id: CHIPS,
            priority: 4,
            variants: OTHER_V,
        },
        PanelSpec {
            id: PSI,
            priority: 5,
            variants: OTHER_V,
        },
    ];

    fn panel(id: PanelId) -> Node {
        Node::Panel(id)
    }

    fn overview() -> Node {
        Node::Col(vec![
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(3), panel(CPU)),
                    (
                        Size::Weight(2),
                        Node::Col(vec![
                            (Size::Weight(1), panel(MEM)),
                            (Size::Weight(1), panel(NET)),
                            (Size::Weight(1), panel(DISK)),
                        ]),
                    ),
                ]),
            ),
            (
                Size::Weight(1),
                Node::Row(vec![
                    (Size::Weight(3), panel(PROCS)),
                    (
                        Size::Min(12),
                        Node::Col(vec![
                            (Size::Weight(1), panel(CHIPS)),
                            (Size::Weight(1), panel(PSI)),
                        ]),
                    ),
                ]),
            ),
        ])
    }

    fn area(cols: usize, rows: usize) -> Rect {
        Rect {
            x: 3,
            y: 5,
            cols,
            rows,
        }
    }

    fn count_cells(solution: &Solution, area: Rect) -> Vec<Vec<u8>> {
        let mut counts = vec![vec![0u8; area.cols]; area.rows];
        for placed in &solution.placed {
            assert!(placed.rect.x >= area.x && placed.rect.y >= area.y);
            assert!(placed.rect.x + placed.rect.cols <= area.x + area.cols);
            assert!(placed.rect.y + placed.rect.rows <= area.y + area.rows);
            let x0 = placed.rect.x - area.x;
            let x1 = x0 + placed.rect.cols;
            let y0 = placed.rect.y - area.y;
            let y1 = y0 + placed.rect.rows;
            for row in &mut counts[y0..y1] {
                for count in &mut row[x0..x1] {
                    *count += 1;
                }
            }
        }
        counts
    }

    #[test]
    fn overview_tiles_every_cell_across_the_sweep() {
        let tree = overview();
        for cols in 56..=300 {
            for rows in 16..=80 {
                let bounds = area(cols, rows);
                let solution = solve(&tree, SPECS, bounds, None, &[]);
                assert_eq!(solution.overflow_rows, 0, "{cols}x{rows}");
                assert!(
                    count_cells(&solution, bounds)
                        .iter()
                        .all(|row| row.iter().all(|count| *count == 1)),
                    "{cols}x{rows}"
                );
            }
        }
    }

    #[test]
    fn shrinking_axes_only_downgrades_or_drops() {
        let tree = overview();
        for rows in 16..=80 {
            let mut larger = solve(&tree, SPECS, area(300, rows), None, &[]);
            for cols in (56..300).rev() {
                let smaller = solve(&tree, SPECS, area(cols, rows), None, &[]);
                assert_monotonic(&larger, &smaller);
                larger = smaller;
            }
        }
        for cols in 56..=300 {
            let mut larger = solve(&tree, SPECS, area(cols, 80), None, &[]);
            for rows in (16..80).rev() {
                let smaller = solve(&tree, SPECS, area(cols, rows), None, &[]);
                assert_monotonic(&larger, &smaller);
                larger = smaller;
            }
        }
    }

    fn assert_monotonic(larger: &Solution, smaller: &Solution) {
        for placed in &smaller.placed {
            if let Some(previous) = larger.placed.iter().find(|item| item.id == placed.id) {
                assert!(rank(placed.variant) >= rank(previous.variant));
            }
        }
        for hidden in &larger.dropped {
            assert!(smaller.dropped.contains(hidden));
            assert!(smaller.placed.iter().all(|placed| placed.id != *hidden));
        }
    }

    fn rank(kind: VariantKind) -> usize {
        match kind {
            VariantKind::Full => 0,
            VariantKind::Compact => 1,
            VariantKind::Mini => 2,
        }
    }

    #[test]
    fn solve_is_deterministic_and_drops_by_priority_without_dropping_cpu() {
        let tree = overview();
        let first = solve(&tree, SPECS, area(30, 5), None, &[]);
        assert_eq!(first, solve(&tree, SPECS, area(30, 5), None, &[]));
        assert_eq!(first.dropped, vec![PSI, CHIPS, DISK, NET, MEM, PROCS]);
        assert!(!first.dropped.contains(&CPU));
        let middle = solve(&tree, SPECS, area(100, 30), None, &[]);
        assert_eq!(middle.hidden(), 0);
        assert!(middle.dropped.is_empty());
        let narrow = solve(&tree, SPECS, area(56, 16), None, &[]);
        assert_eq!(narrow.dropped, vec![PSI, CHIPS, DISK]);
        assert_eq!(
            narrow
                .placed
                .iter()
                .find(|placed| placed.id == NET)
                .map(|placed| placed.variant),
            Some(VariantKind::Mini)
        );
        assert!(!narrow.dropped.contains(&CPU));

        let mut larger = solve(&tree, SPECS, area(300, 80), None, &[]);
        for step in 1..=244 {
            let cols = 300 - step;
            let rows = 80 - ((step * 64) / 244);
            let smaller = solve(&tree, SPECS, area(cols, rows), None, &[]);
            assert!(smaller.dropped.starts_with(&larger.dropped));
            assert_dropped_by_priority(&smaller.dropped);
            larger = smaller;
        }
        assert_eq!(larger.dropped, narrow.dropped);
    }

    fn assert_dropped_by_priority(dropped: &[PanelId]) {
        let priorities = dropped
            .iter()
            .map(|id| SPECS.iter().find(|spec| spec.id == *id).unwrap().priority)
            .collect::<Vec<_>>();
        assert!(priorities.windows(2).all(|pair| pair[0] >= pair[1]));
    }

    #[test]
    fn absent_panel_is_removed_and_its_sibling_redistributes() {
        let tree = Node::Row(vec![
            (Size::Weight(1), panel(NET)),
            (Size::Weight(1), panel(DISK)),
        ]);
        let present = solve(&tree, SPECS, area(80, 16), None, &[]);
        let absent = solve(&tree, SPECS, area(80, 16), None, &[NET]);
        assert!(absent.placed.iter().all(|placed| placed.id != NET));
        assert!(!absent.dropped.contains(&NET));
        assert!(absent.placed[0].rect.cols > present.placed[1].rect.cols);
    }

    #[test]
    fn zoom_uses_richest_fitting_variant_and_area() {
        let bounds = area(20, 5);
        let solution = solve(&overview(), SPECS, bounds, Some(CPU), &[]);
        assert_eq!(
            solution.placed,
            vec![super::Placed {
                id: CPU,
                rect: bounds,
                variant: VariantKind::Compact
            }]
        );
        assert!(solution.dropped.is_empty());
    }

    #[test]
    fn exhausted_priority_zero_panels_stack_in_tree_order() {
        static TOO_LARGE: &[Variant] = &[Variant {
            kind: VariantKind::Mini,
            min: (40, 12),
            pref: (40, 12),
        }];
        static P0: &[PanelSpec] = &[
            PanelSpec {
                id: CPU,
                priority: 0,
                variants: TOO_LARGE,
            },
            PanelSpec {
                id: MEM,
                priority: 0,
                variants: TOO_LARGE,
            },
        ];
        let tree = Node::Row(vec![
            (Size::Weight(1), panel(MEM)),
            (Size::Weight(1), panel(CPU)),
        ]);
        let solution = solve(&tree, P0, area(30, 10), None, &[]);
        assert!(solution.overflow_rows > 0);
        assert_eq!(
            solution
                .placed
                .iter()
                .map(|placed| placed.id)
                .collect::<Vec<_>>(),
            vec![MEM, CPU]
        );
    }

    #[test]
    fn weighted_remainders_and_fixed_remainder_are_exact() {
        static TINY: &[Variant] = &[Variant {
            kind: VariantKind::Mini,
            min: (1, 1),
            pref: (1, 1),
        }];
        static TINY_SPECS: &[PanelSpec] = &[
            PanelSpec {
                id: CPU,
                priority: 0,
                variants: TINY,
            },
            PanelSpec {
                id: MEM,
                priority: 0,
                variants: TINY,
            },
        ];
        let tree = Node::Row(vec![
            (Size::Weight(1), panel(CPU)),
            (Size::Weight(2), panel(MEM)),
        ]);
        let weighted = solve(&tree, TINY_SPECS, area(31, 16), None, &[]);
        assert_eq!(
            weighted
                .placed
                .iter()
                .map(|placed| placed.rect.cols)
                .collect::<Vec<_>>(),
            vec![11, 20]
        );
        let tie = Node::Row(vec![
            (Size::Weight(1), panel(CPU)),
            (Size::Weight(1), panel(MEM)),
        ]);
        let tied = solve(&tie, TINY_SPECS, area(31, 16), None, &[]);
        assert_eq!(
            tied.placed
                .iter()
                .map(|placed| placed.rect.cols)
                .collect::<Vec<_>>(),
            vec![16, 15]
        );
        let fixed = Node::Row(vec![
            (Size::Fixed(12), panel(CPU)),
            (Size::Fixed(12), panel(MEM)),
        ]);
        let fixed_solution = solve(&fixed, TINY_SPECS, area(31, 16), None, &[]);
        assert_eq!(
            fixed_solution
                .placed
                .iter()
                .map(|placed| placed.rect.cols)
                .collect::<Vec<_>>(),
            vec![12, 19]
        );
        let minimum = Node::Row(vec![(Size::Min(4), panel(CPU)), (Size::Min(1), panel(MEM))]);
        let minimum_solution = solve(&minimum, TINY_SPECS, area(31, 16), None, &[]);
        assert_eq!(
            minimum_solution
                .placed
                .iter()
                .map(|placed| placed.rect.cols)
                .collect::<Vec<_>>(),
            vec![4, 27]
        );
    }
}
