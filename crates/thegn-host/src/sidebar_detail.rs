//! Pure composition and width fitting for worktree detail lines.

use std::time::{SystemTime, UNIX_EPOCH};

use thegn_core::config::DetailField;
use thegn_core::pipeline_report::display_cell;
use thegn_core::theme;
use thegn_svc::git::{DiffCount, META_COUNT_CAP};

use crate::chrome::S;
use crate::seg::{Line, Seg, Tok, seg, seg_width, sp};
use crate::sidebar::{RowKind, SidebarRow};
use crate::sidebar_view::SidebarDisplay;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldId {
    Branch,
    Commits,
    Dirty,
    BranchStat,
    Pr,
    Subject,
    Age,
    Upstream,
    Stage,
    Activity,
    Author,
    TailEnv,
    TailSandbox,
    TailDevcontainer,
    TailHibernated,
    TailMq,
    TailUnread,
    TailDisk,
}

#[derive(Debug, Clone)]
pub(crate) struct Field {
    pub(crate) id: FieldId,
    pub(crate) segs: Vec<Seg>,
    pub(crate) width: usize,
    pub(crate) min_width: usize,
}

impl Field {
    fn new(id: FieldId, segs: Vec<Seg>, min_width: usize) -> Self {
        let width = seg_width(&segs);
        Self {
            id,
            segs,
            width,
            min_width,
        }
    }
}

/// Current Unix time used only when composing a visible detail line.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64)
}

/// Format an approximate, idle-stable age using the largest useful unit.
pub(crate) fn fmt_age(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    if secs < 60 {
        "<1m".into()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 604_800 {
        format!("{}d", secs / 86_400)
    } else if secs < 4_838_400 {
        format!("{}w", secs / 604_800)
    } else if secs < 31_536_000 {
        format!("{}mo", secs / 2_592_000)
    } else {
        format!("{}y", secs / 31_536_000)
    }
}

fn field_id(field: DetailField) -> FieldId {
    match field {
        DetailField::Branch => FieldId::Branch,
        DetailField::Commits => FieldId::Commits,
        DetailField::Dirty => FieldId::Dirty,
        DetailField::BranchStat => FieldId::BranchStat,
        DetailField::Pr => FieldId::Pr,
        DetailField::Subject => FieldId::Subject,
        DetailField::Age => FieldId::Age,
        DetailField::Upstream => FieldId::Upstream,
        DetailField::Stage => FieldId::Stage,
        DetailField::Activity => FieldId::Activity,
        DetailField::Env => FieldId::TailEnv,
        DetailField::Sandbox => FieldId::TailSandbox,
        DetailField::Devcontainer => FieldId::TailDevcontainer,
        DetailField::Hibernated => FieldId::TailHibernated,
        DetailField::Mq => FieldId::TailMq,
        DetailField::Unread => FieldId::TailUnread,
        DetailField::Disk => FieldId::TailDisk,
        DetailField::Author => FieldId::Author,
    }
}

fn count_text(count: u32) -> String {
    if count >= META_COUNT_CAP {
        "999+".into()
    } else {
        count.to_string()
    }
}

/// Substitute every `{n}` occurrence, including compact or repeated templates.
pub(crate) fn format_badge(template: &str, count: u32, fallback: &str) -> String {
    let template = if template.contains("{n}") {
        template
    } else {
        fallback
    };
    display_cell(&template.replace("{n}", &count_text(count)))
}

fn detail_fields(row: &SidebarRow, disp: &SidebarDisplay, now_unix: i64) -> Vec<Field> {
    if row.kind != RowKind::Worktree {
        return Vec::new();
    }
    let branch = row
        .branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
        .unwrap_or(&row.label);
    if disp.detail_fields.is_empty() {
        return branch_field(row, branch, FieldId::Branch)
            .into_iter()
            .collect();
    }

    disp.detail_fields
        .iter()
        .flat_map(|requested| requested_fields(row, disp, *requested, branch, now_unix))
        .collect()
}

fn branch_field(row: &SidebarRow, branch: &str, id: FieldId) -> Option<Field> {
    (row.kind == RowKind::Worktree && !branch.is_empty())
        .then(|| Field::new(id, vec![seg(Tok::Slot(S::Dim), display_cell(branch))], 1))
}

fn requested_fields(
    row: &SidebarRow,
    disp: &SidebarDisplay,
    requested: DetailField,
    branch: &str,
    now_unix: i64,
) -> Vec<Field> {
    let id = field_id(requested);
    match requested {
        DetailField::Branch if disp.detail_branch => {
            branch_field(row, branch, id).into_iter().collect()
        }
        DetailField::Commits => {
            let mut segs = Vec::new();
            if disp.show_commits
                && let Some(count) = row.branch_meta.as_ref().and_then(|m| m.ahead_base)
                && count > 0
            {
                segs.push(seg(
                    Tok::Slot(S::Dim),
                    format_badge(&disp.commit_badge, count, "{n}c"),
                ));
            }
            if disp.show_behind_base
                && let Some(count) = row.branch_meta.as_ref().and_then(|m| m.behind_base)
                && count > 0
            {
                if !segs.is_empty() {
                    segs.push(sp(1));
                }
                segs.push(seg(
                    Tok::Hue(theme::Hue::Amber),
                    format_badge(&disp.behind_badge, count, "{n}b"),
                ));
            }
            (!segs.is_empty())
                .then(|| Field::new(id, segs, 1))
                .into_iter()
                .collect()
        }
        DetailField::Dirty => match row.branch_meta.as_ref().and_then(|m| m.dirty_files) {
            Some(count) if count > 0 => vec![Field::new(
                id,
                vec![seg(
                    Tok::Hue(theme::Hue::Amber),
                    format!("{}{}", disp.status_glyph(), count),
                )],
                1,
            )],
            Some(_) => Vec::new(),
            None => row
                .git
                .is_some_and(|g| g.dirty)
                .then(|| {
                    Field::new(
                        id,
                        vec![seg(Tok::Hue(theme::Hue::Amber), disp.status_glyph())],
                        1,
                    )
                })
                .into_iter()
                .collect(),
        },
        DetailField::BranchStat if disp.detail_branch_stat => {
            branch_stat_field(row, id).into_iter().collect()
        }
        DetailField::Pr if disp.detail_pr => pr_field(row, id).into_iter().collect(),
        DetailField::Subject => row
            .branch_meta
            .as_ref()
            .and_then(|meta| meta.last_subject.as_deref())
            .filter(|subject| !subject.is_empty())
            .map(|subject| Field::new(id, vec![seg(Tok::Slot(S::Dim), display_cell(subject))], 8))
            .into_iter()
            .collect(),
        DetailField::Age => row
            .branch_meta
            .as_ref()
            .and_then(|meta| meta.last_commit_unix)
            .map(|stamp| {
                Field::new(
                    id,
                    vec![seg(
                        Tok::Slot(S::Faint),
                        fmt_age(now_unix.saturating_sub(stamp)),
                    )],
                    1,
                )
            })
            .into_iter()
            .collect(),
        DetailField::Upstream => upstream_fields(row, id),
        DetailField::Stage => row
            .pipeline_stage
            .as_deref()
            .filter(|stage| !stage.trim().is_empty())
            .map(|stage| Field::new(id, vec![seg(Tok::Slot(S::Dim), display_cell(stage))], 1))
            .into_iter()
            .collect(),
        DetailField::Activity => match row.activity {
            crate::sidebar::ActivityState::Active => vec![Field::new(
                id,
                vec![seg(Tok::Slot(S::ActivityActive), "active")],
                1,
            )],
            crate::sidebar::ActivityState::Waiting => vec![Field::new(
                id,
                vec![seg(Tok::Slot(S::ActivityWaiting), "waiting")],
                1,
            )],
            _ => Vec::new(),
        },
        DetailField::Env => env_fields(row),
        DetailField::Sandbox => sandbox_fields(row),
        DetailField::Devcontainer => devcontainer_fields(row),
        DetailField::Hibernated => hibernated_fields(row),
        DetailField::Mq => mq_fields(row),
        DetailField::Unread => unread_fields(row),
        DetailField::Disk => disk_fields(row),
        DetailField::Author => row
            .branch_meta
            .as_ref()
            .and_then(|meta| meta.last_author.as_deref())
            .filter(|author| !author.is_empty())
            .map(|author| Field::new(id, vec![seg(Tok::Slot(S::Dim), display_cell(author))], 1))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn branch_stat_field(row: &SidebarRow, id: FieldId) -> Option<Field> {
    let (add, del) = row.git.and_then(|git| git.branch_diff)?;
    let mut segs = Vec::new();
    if add == DiffCount::Unavailable || del == DiffCount::Unavailable {
        segs.push(seg(Tok::Hue(theme::Hue::Amber), "diff ?"));
    } else {
        if add == DiffCount::Binary || del == DiffCount::Binary {
            segs.push(seg(Tok::Slot(S::Dim), "bin"));
        }
        if let DiffCount::Exact(count) = add
            && count > 0
        {
            if !segs.is_empty() {
                segs.push(sp(1));
            }
            segs.push(seg(Tok::Hue(theme::Hue::Green), format!("+{count}")));
        } else if add == DiffCount::Capped {
            if !segs.is_empty() {
                segs.push(sp(1));
            }
            segs.push(seg(Tok::Hue(theme::Hue::Green), "+capped"));
        }
        if let DiffCount::Exact(count) = del
            && count > 0
        {
            if !segs.is_empty() {
                segs.push(sp(1));
            }
            segs.push(seg(Tok::Hue(theme::Hue::Red), format!("-{count}")));
        } else if del == DiffCount::Capped {
            if !segs.is_empty() {
                segs.push(sp(1));
            }
            segs.push(seg(Tok::Hue(theme::Hue::Red), "-capped"));
        }
    }
    (!segs.is_empty()).then(|| Field::new(id, segs, 1))
}

fn pr_field(row: &SidebarRow, id: FieldId) -> Option<Field> {
    let gl = crate::caps::active_glyphs();
    let text = row
        .pr_number
        .map(|number| format!("{}#{number}", gl.hex))
        .or_else(|| {
            row.pr_count
                .filter(|count| *count > 0)
                .map(|count| format!("{}{count}", gl.hex))
        })?;
    Some(Field::new(
        id,
        vec![seg(Tok::Hue(theme::Hue::Green), text)],
        1,
    ))
}

fn upstream_fields(row: &SidebarRow, id: FieldId) -> Vec<Field> {
    let Some(git) = row.git else {
        return Vec::new();
    };
    if git.ahead == 0 && git.behind == 0 {
        return Vec::new();
    }
    let gl = crate::caps::active_glyphs();
    vec![Field::new(
        id,
        vec![
            seg(Tok::Slot(S::Dim), format!("{}{}", gl.arrow_up, git.ahead)),
            seg(
                Tok::Hue(theme::Hue::Amber),
                format!("{}{}", gl.arrow_down, git.behind),
            ),
        ],
        1,
    )]
}

fn env_fields(row: &SidebarRow) -> Vec<Field> {
    let gl = crate::caps::active_glyphs();
    if let Some(env) = &row.env_name
        && !env.is_empty()
        && env != "default"
    {
        let text = if row.env_degraded {
            format!(
                "{}{} {}{}",
                gl.quote_open,
                display_cell(env),
                gl.cross,
                gl.quote_close
            )
        } else {
            format!("{}{}{}", gl.quote_open, display_cell(env), gl.quote_close)
        };
        let fg = if row.env_degraded {
            Tok::Hue(theme::Hue::Amber)
        } else {
            Tok::Slot(S::Faint)
        };
        return vec![Field::new(FieldId::TailEnv, vec![seg(fg, text)], 1)];
    }
    Vec::new()
}

fn sandbox_fields(row: &SidebarRow) -> Vec<Field> {
    let mut fields = Vec::new();
    if let Some(profile) = &row.sandbox_profile {
        fields.push(Field::new(
            FieldId::TailSandbox,
            vec![seg(
                Tok::Slot(S::Faint),
                format!("[{}]", display_cell(profile)),
            )],
            1,
        ));
    }
    if let Some(backend) = &row.sandbox_backend
        && !backend.is_empty()
        && backend != "none"
        && backend != "host"
    {
        fields.push(Field::new(
            FieldId::TailSandbox,
            vec![seg(
                Tok::Slot(S::Faint),
                format!("({})", display_cell(backend)),
            )],
            1,
        ));
    }
    fields
}

fn devcontainer_fields(row: &SidebarRow) -> Vec<Field> {
    if let Some(status) = &row.devcontainer_status {
        return vec![Field::new(
            FieldId::TailDevcontainer,
            vec![seg(Tok::Slot(S::Faint), display_cell(status))],
            1,
        )];
    }
    Vec::new()
}

fn hibernated_fields(row: &SidebarRow) -> Vec<Field> {
    let gl = crate::caps::active_glyphs();
    if row
        .worktree_path
        .as_deref()
        .is_some_and(crate::hibernator::is_hibernated)
    {
        return vec![Field::new(
            FieldId::TailHibernated,
            vec![seg(Tok::Slot(S::Faint), gl.moon)],
            1,
        )];
    }
    Vec::new()
}

fn mq_fields(row: &SidebarRow) -> Vec<Field> {
    if let Some((glyph, hue)) = row.mq_status.and_then(crate::sidebar::mq_chip) {
        return vec![Field::new(
            FieldId::TailMq,
            vec![seg(Tok::Hue(hue), glyph)],
            1,
        )];
    }
    Vec::new()
}

fn unread_fields(row: &SidebarRow) -> Vec<Field> {
    let gl = crate::caps::active_glyphs();
    if row.unread_count > 0 {
        return vec![Field::new(
            FieldId::TailUnread,
            vec![seg(
                Tok::Hue(theme::Hue::Blue),
                format!("{}{}", gl.mail, row.unread_count),
            )],
            1,
        )];
    }
    Vec::new()
}

fn disk_fields(row: &SidebarRow) -> Vec<Field> {
    if let Some(total) = row.disk_bytes {
        let target = row.target_bytes.unwrap_or(0);
        let heavy = target > 1024 * 1024 * 1024 && target.saturating_mul(2) > total;
        let fg = if heavy {
            Tok::Hue(theme::Hue::Amber)
        } else {
            Tok::Slot(S::Dim)
        };
        return vec![Field::new(
            FieldId::TailDisk,
            vec![seg(fg, thegn_core::disk::human(total))],
            1,
        )];
    }
    Vec::new()
}

fn field_width(fields: &[Field]) -> usize {
    fields
        .iter()
        .map(|field| field.width)
        .sum::<usize>()
        .saturating_add(fields.len().saturating_sub(1) * 2)
}

fn truncate_field(field: &mut Field, max: usize) {
    field.segs = crate::seg::cut(&field.segs, max);
    field.width = seg_width(&field.segs);
}

/// Fit fields by dropping the least important content first. The final field
/// is always retained and clipped, so content presence never changes at the
/// width boundary.
pub(crate) fn fit_fields(mut fields: Vec<Field>, budget: usize) -> Vec<Field> {
    if fields.is_empty() {
        return fields;
    }
    if field_width(&fields) <= budget {
        return fields;
    }

    if let Some(field) = fields.iter_mut().find(|field| field.id == FieldId::Subject) {
        truncate_field(field, field.min_width);
    }

    let drop_order = [
        FieldId::Activity,
        FieldId::Stage,
        FieldId::Upstream,
        FieldId::Author,
        FieldId::Subject,
        FieldId::Age,
        FieldId::TailDisk,
        FieldId::TailUnread,
        FieldId::TailMq,
        FieldId::TailHibernated,
        FieldId::TailDevcontainer,
        FieldId::TailSandbox,
        FieldId::TailEnv,
        FieldId::BranchStat,
        FieldId::Pr,
        FieldId::Dirty,
        FieldId::Commits,
    ];
    for id in drop_order {
        while field_width(&fields) > budget {
            let Some(index) = fields.iter().position(|field| field.id == id) else {
                break;
            };
            if fields.len() == 1 {
                break;
            }
            fields.remove(index);
        }
    }
    if field_width(&fields) > budget {
        let branch_index = fields
            .iter()
            .position(|field| field.id == FieldId::Branch)
            .unwrap_or(fields.len().saturating_sub(1));
        if fields.len() > 1 {
            let keep = fields.remove(branch_index);
            fields.clear();
            fields.push(keep);
        }
        let max = budget.saturating_sub(0);
        if let Some(field) = fields.first_mut() {
            truncate_field(field, max);
        }
    }
    fields
}

/// Compose a detail line using the caller's measured indentation and width.
pub(crate) fn compose_detail_line_at(
    row: &SidebarRow,
    disp: &SidebarDisplay,
    now_unix: i64,
    indent: usize,
    cols: usize,
) -> Option<Line> {
    let indent = indent.min(cols / 2);
    let fields = fit_fields(
        detail_fields(row, disp, now_unix),
        cols.saturating_sub(indent),
    );
    if fields.is_empty() {
        return None;
    }
    let mut segs = vec![sp(indent)];
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            segs.push(sp(2));
        }
        segs.extend(field.segs);
    }
    Some(Line::Segs(segs))
}

/// The width-independent content predicate used by the heights pass.
pub(crate) fn detail_has_content(row: &SidebarRow, disp: &SidebarDisplay) -> bool {
    !detail_fields(row, disp, 0).is_empty()
}

/// Stable wrapper retained for sidebar callers and unit tests.
#[cfg(test)]
pub(crate) fn compose_detail_line(row: &SidebarRow, disp: &SidebarDisplay) -> Option<Line> {
    compose_detail_line_at(
        row,
        disp,
        unix_now(),
        4 + (row.depth.saturating_sub(1) as usize) * 2 + 2 + 2,
        80,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_svc::git::BranchMeta;

    fn row() -> SidebarRow {
        let mut row = SidebarRow::base(RowKind::Worktree, 1, "feature", "repo");
        row.branch = Some("feature/retry-loop".into());
        row.branch_meta = Some(BranchMeta {
            ahead_base: Some(5),
            behind_base: Some(2),
            dirty_files: Some(3),
            last_commit_unix: Some(1_000),
            last_subject: Some("fix retry loop".into()),
            last_author: Some("Ada Lovelace".into()),
        });
        row
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(0), "<1m");
        assert_eq!(fmt_age(300), "5m");
        assert_eq!(fmt_age(10_800), "3h");
        assert_eq!(fmt_age(172_800), "2d");
        assert_eq!(fmt_age(3_628_800), "6w");
        assert_eq!(fmt_age(10_368_000), "4mo");
        assert_eq!(fmt_age(63_072_000), "2y");
    }

    #[test]
    fn badge_templates_replace_all_occurrences_and_cap() {
        assert_eq!(format_badge("{n}c", 5, "{n}c"), "5c");
        assert_eq!(format_badge("{n}{n}", 1_000, "{n}c"), "999+999+");
        assert_eq!(format_badge("odd", 2, "{n}b"), "2b");
    }

    #[test]
    fn badge_templates_strip_control_chars() {
        let rendered = format_badge("\u{1b}[31m{n}", 2, "{n}c");
        assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
    }

    #[test]
    fn dirty_detail_falls_back_to_git_signal_without_meta() {
        let mut row = row();
        row.branch_meta = None;
        row.git = Some(crate::sidebar::GitGlyphs {
            dirty: true,
            ..crate::sidebar::GitGlyphs::default()
        });
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Dirty],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        assert!(
            segs.iter()
                .any(|segment| segment.text == display.status_glyph())
        );
    }

    #[test]
    fn degraded_environment_badge_is_amber() {
        let mut row = row();
        row.env_name = Some("host".into());
        row.env_degraded = true;
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Env],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        assert!(segs.iter().any(|segment| {
            segment.text.contains("host") && segment.fg == Tok::Hue(theme::Hue::Amber)
        }));
    }

    #[test]
    fn empty_detail_list_keeps_only_the_branch_fallback() {
        let mut row = row();
        row.env_name = Some("host".into());
        row.disk_bytes = Some(10_000);
        let display = SidebarDisplay {
            detail_fields: Vec::new(),
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        let text = segs
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert_eq!(text, "feature/retry-loop");
    }

    #[test]
    fn new_fields_render_when_present_and_omit_absent_values() {
        let mut row = row();
        row.git = Some(crate::sidebar::GitGlyphs {
            ahead: 2,
            behind: 1,
            ..crate::sidebar::GitGlyphs::default()
        });
        row.pipeline_stage = Some("review".into());
        row.activity = crate::sidebar::ActivityState::Active;
        row.env_name = Some("dev".into());
        row.sandbox_profile = Some("strict".into());
        row.sandbox_backend = Some("bwrap".into());
        row.devcontainer_status = Some("running".into());
        row.mq_status = Some(thegn_core::attention::MqStatus::Queued);
        row.unread_count = 3;
        row.disk_bytes = Some(10_000);
        let display = SidebarDisplay {
            detail_fields: vec![
                DetailField::Upstream,
                DetailField::Stage,
                DetailField::Activity,
                DetailField::Env,
                DetailField::Sandbox,
                DetailField::Devcontainer,
                DetailField::Hibernated,
                DetailField::Mq,
                DetailField::Unread,
                DetailField::Disk,
                DetailField::Author,
            ],
            ..SidebarDisplay::default()
        };
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000)
            .into_iter()
            .map(|field| field.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                FieldId::Upstream,
                FieldId::Stage,
                FieldId::Activity,
                FieldId::TailEnv,
                FieldId::TailSandbox,
                FieldId::TailSandbox,
                FieldId::TailDevcontainer,
                FieldId::TailMq,
                FieldId::TailUnread,
                FieldId::TailDisk,
                FieldId::Author,
            ]
        );

        row.pipeline_stage = None;
        row.activity = crate::sidebar::ActivityState::Read;
        row.env_name = None;
        row.sandbox_profile = None;
        row.sandbox_backend = None;
        row.devcontainer_status = None;
        row.mq_status = None;
        row.unread_count = 0;
        row.disk_bytes = None;
        row.git = Some(crate::sidebar::GitGlyphs::default());
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000)
            .into_iter()
            .map(|field| field.id)
            .collect();
        assert_eq!(ids, vec![FieldId::Author]);
    }

    #[test]
    fn user_order_can_put_environment_before_branch_and_disk_can_be_dropped() {
        let mut row = row();
        row.env_name = Some("dev".into());
        row.disk_bytes = Some(10_000);
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Env, DetailField::Branch],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        let text = segs
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(text.find("dev").unwrap() < text.find("feature/retry").unwrap());

        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Branch],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        let text = segs
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert_eq!(text, "feature/retry-loop");
    }

    #[test]
    fn legacy_branch_stat_toggle_hides_listed_stat() {
        let mut row = row();
        row.git = Some(crate::sidebar::GitGlyphs {
            branch_diff: Some((DiffCount::Exact(1), DiffCount::Exact(2))),
            ..crate::sidebar::GitGlyphs::default()
        });
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::BranchStat],
            detail_branch_stat: false,
            ..SidebarDisplay::default()
        };
        assert!(compose_detail_line_at(&row, &display, 2_000, 0, 80).is_none());
    }

    #[test]
    fn fit_fields_drops_in_priority_order() {
        let ids = [
            FieldId::Branch,
            FieldId::Commits,
            FieldId::Dirty,
            FieldId::Pr,
            FieldId::BranchStat,
            FieldId::Age,
            FieldId::Subject,
            FieldId::TailDisk,
        ];
        for budget in [80, 40, 24, 16, 8] {
            let fields = ids
                .into_iter()
                .map(|id| Field::new(id, vec![seg(Tok::Slot(S::Dim), "1234567890")], 8))
                .collect();
            let fitted = fit_fields(fields, budget);
            assert!(field_width(&fitted) <= budget, "{budget}: {fitted:?}");
            assert!(!fitted.is_empty(), "{budget}");
            assert!(fitted.iter().any(|field| field.id == FieldId::Branch));
            let got: Vec<FieldId> = fitted.iter().map(|field| field.id).collect();
            let expected: &[FieldId] = match budget {
                80 => &[
                    FieldId::Branch,
                    FieldId::Commits,
                    FieldId::Dirty,
                    FieldId::Pr,
                    FieldId::BranchStat,
                    FieldId::TailDisk,
                ],
                40 => &[FieldId::Branch, FieldId::Commits, FieldId::Dirty],
                24 => &[FieldId::Branch, FieldId::Commits],
                16 | 8 => &[FieldId::Branch],
                _ => unreachable!(),
            };
            assert_eq!(got, expected, "{budget}: {fitted:?}");
        }
    }

    #[test]
    fn detail_has_content_matches_compose_is_some() {
        let row = row();
        let display = SidebarDisplay::default();
        assert_eq!(
            detail_has_content(&row, &display),
            compose_detail_line_at(&row, &display, 2_000, 8, 24).is_some()
        );
    }

    #[test]
    fn detail_fields_order_is_respected() {
        let row = row();
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Subject, DetailField::Branch],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        let text = segs
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(text.find("fix retry").unwrap() < text.find("feature/retry").unwrap());
    }

    #[test]
    fn legacy_detail_pr_false_hides_pr_even_when_listed() {
        let mut row = row();
        row.pr_number = Some(42);
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Pr],
            detail_pr: false,
            ..SidebarDisplay::default()
        };
        assert!(compose_detail_line_at(&row, &display, 2_000, 0, 80).is_none());
    }

    #[test]
    fn no_line_wider_than_cols() {
        let row = row();
        let display = SidebarDisplay::default();
        for cols in 8..=80 {
            let line = compose_detail_line_at(&row, &display, 2_000, 8, cols).unwrap();
            let Line::Segs(segs) = line else {
                panic!("detail line is a segment line")
            };
            assert!(seg_width(&segs) <= cols, "{cols}: {segs:?}");
        }
    }

    #[test]
    fn subject_truncates_with_ellipsis_and_wide_chars() {
        let mut row = row();
        row.branch_meta.as_mut().unwrap().last_subject = Some("你好世界 retry".into());
        let display = SidebarDisplay {
            detail_fields: vec![DetailField::Subject],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 8).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        let text = segs
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(
            text.contains(crate::caps::active_glyphs().ellipsis),
            "{text:?}"
        );
        assert!(seg_width(&segs) <= 8);
    }
}
