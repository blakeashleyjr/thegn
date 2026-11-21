//! Pure composition and width fitting for worktree detail lines.

use std::time::{SystemTime, UNIX_EPOCH};

use thegn_core::config::{DetailEntry, DetailField};
use thegn_core::pipeline_report::display_cell;
use thegn_core::plugin_api::{RowFieldSlot, RowRole};
use thegn_core::theme;
use thegn_svc::git::{DiffCount, META_COUNT_CAP};

use crate::chrome::S;
use crate::seg::{Line, Seg, Tok, seg, seg_width, sp};
use crate::sidebar::{CiBadge, PluginFieldValue, SidebarRow};
use crate::sidebar_view::SidebarDisplay;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldId {
    Plugin,
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
    Stash,
    Ci,
    Issue,
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
        DetailField::Stash => FieldId::Stash,
        DetailField::Ci => FieldId::Ci,
        DetailField::Issue => FieldId::Issue,
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

/// What the worktree's title line already shows; the detail line never repeats it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TitleEcho {
    pub branch: bool,
    pub commits: bool,
    pub pr: bool,
    pub upstream: bool,
}

/// The branch `detail_fields` shows: the row's branch, else its label.
fn detail_branch(row: &SidebarRow) -> &str {
    row.branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
        .unwrap_or(&row.label)
}

/// Which detail fields the title line already carries. Width-independent, so
/// the heights pass and the compose pass agree.
pub(crate) fn title_echo(row: &SidebarRow, disp: &SidebarDisplay, title: &str) -> TitleEcho {
    TitleEcho {
        branch: row.kind.is_worktree() && display_cell(title) == display_cell(detail_branch(row)),
        commits: disp.show_commits || disp.show_behind_base,
        pr: disp.show_pr_chip && row.pr_number.is_some(),
        upstream: disp.show_ahead_behind && row.git.is_some(),
    }
}

fn detail_fields(
    row: &SidebarRow,
    disp: &SidebarDisplay,
    now_unix: i64,
    echo: TitleEcho,
) -> Vec<Field> {
    if !row.kind.is_worktree() {
        return Vec::new();
    }
    let branch = detail_branch(row);
    if disp.detail_fields.is_empty() {
        if echo.branch {
            return Vec::new();
        }
        return branch_field(row, branch, FieldId::Branch)
            .into_iter()
            .collect();
    }

    disp.detail_fields
        .iter()
        .flat_map(|requested| match requested {
            DetailEntry::Builtin(field) => requested_fields(row, disp, *field, branch, now_unix),
            DetailEntry::Plugin { plugin, field } => plugin_fields(row, plugin, field),
        })
        .filter(|field| match field.id {
            FieldId::Branch => !echo.branch,
            FieldId::Commits => !echo.commits,
            FieldId::Pr => !echo.pr,
            FieldId::Upstream => !echo.upstream,
            _ => true,
        })
        .collect()
}

pub(crate) fn plugin_role(role: RowRole) -> Tok {
    match role {
        RowRole::Accent => Tok::Slot(S::Accent),
        RowRole::Dim => Tok::Slot(S::Dim),
        RowRole::Good => Tok::Hue(theme::Hue::Green),
        RowRole::Warn => Tok::Hue(theme::Hue::Amber),
        RowRole::Bad => Tok::Hue(theme::Hue::Red),
        RowRole::Unknown => Tok::Slot(S::Dim),
    }
}

fn plugin_fields(row: &SidebarRow, plugin: &str, field: &str) -> Vec<Field> {
    row.plugin_fields
        .iter()
        .filter(|value| {
            value.plugin == plugin
                && value.field == field
                && value.slot == RowFieldSlot::Detail
                && !display_cell(&value.text).is_empty()
        })
        .map(|value: &PluginFieldValue| {
            Field::new(
                FieldId::Plugin,
                vec![seg(plugin_role(value.role), display_cell(&value.text))],
                1,
            )
        })
        .collect()
}

fn branch_field(row: &SidebarRow, branch: &str, id: FieldId) -> Option<Field> {
    (row.kind.is_worktree() && !branch.is_empty())
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
        DetailField::Stash => row
            .branch_meta
            .as_ref()
            .and_then(|meta| meta.stash)
            .filter(|count| *count > 0)
            .map(|count| {
                Field::new(
                    id,
                    vec![seg(
                        Tok::Slot(S::Dim),
                        format!("stash {}", count_text(count)),
                    )],
                    1,
                )
            })
            .into_iter()
            .collect(),
        DetailField::Ci => row
            .ci_state
            .map(|state| ci_field(state, id))
            .into_iter()
            .collect(),
        DetailField::Issue => row
            .linked_issue
            .as_ref()
            .and_then(|(issue, count)| issue_field(issue, *count, id))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// The `ci` detail field: glyph-free text for the latest cached CI result,
/// coloured by the same roles plugin fields use (a failure takes the error
/// role).
fn ci_field(state: CiBadge, id: FieldId) -> Field {
    let (role, text) = match state {
        CiBadge::Pass => (RowRole::Good, "ci pass"),
        CiBadge::Fail => (RowRole::Bad, "ci fail"),
        CiBadge::Running => (RowRole::Warn, "ci run"),
        CiBadge::Pending => (RowRole::Dim, "ci pend"),
    };
    Field::new(id, vec![seg(plugin_role(role), text)], 1)
}

/// Display cap (cells) for a linked issue id before the `#` prefix and `+N`.
const ISSUE_ID_MAX_CELLS: usize = 24;

/// The `issue` detail field: the newest linked issue id (`#123` for an
/// all-digit id, otherwise the id as stored) plus ` +N` when N more issues are
/// linked. Control characters are stripped, the id is clipped to
/// [`ISSUE_ID_MAX_CELLS`], and an id that sanitises to nothing hides the field.
fn issue_field(issue: &str, count: usize, id: FieldId) -> Option<Field> {
    let stripped: String = issue.chars().filter(|c| !c.is_control()).collect();
    let clean = display_cell(stripped.trim());
    if clean.is_empty() {
        return None;
    }
    let clipped = crate::seg::clip_end(&clean, ISSUE_ID_MAX_CELLS);
    let mut text = if clean.chars().all(|c| c.is_ascii_digit()) {
        format!("#{clipped}")
    } else {
        clipped
    };
    if count > 1 {
        text.push_str(&format!(" +{}", count - 1));
    }
    Some(Field::new(id, vec![seg(Tok::Slot(S::Dim), text)], 1))
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
        FieldId::Plugin,
        FieldId::Activity,
        FieldId::Stage,
        FieldId::Upstream,
        FieldId::Author,
        FieldId::Stash,
        FieldId::Ci,
        FieldId::Issue,
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
/// Test-only since the sidebar paints through [`compose_detail_line_guided`].
#[cfg(test)]
pub(crate) fn compose_detail_line_at(
    row: &SidebarRow,
    disp: &SidebarDisplay,
    now_unix: i64,
    indent: usize,
    cols: usize,
) -> Option<Line> {
    let indent = indent.min(cols / 2);
    compose_detail_line_guided(
        row,
        disp,
        now_unix,
        TitleEcho::default(),
        vec![sp(indent)],
        indent,
        cols,
    )
}

/// Like `compose_detail_line_at`, but the line starts with `lead` (display
/// width `lead_cols`), typically a tree guide, instead of plain spaces. When
/// `lead_cols` exceeds half the column (the clamp `compose_detail_line_at`
/// applies) the guide would misalign, so it falls back to a plain-space lead.
pub(crate) fn compose_detail_line_guided(
    row: &SidebarRow,
    disp: &SidebarDisplay,
    now_unix: i64,
    echo: TitleEcho,
    lead: Vec<Seg>,
    lead_cols: usize,
    cols: usize,
) -> Option<Line> {
    let (lead, lead_cols) = if lead_cols > cols / 2 {
        let clamped = cols / 2;
        (vec![sp(clamped)], clamped)
    } else {
        (lead, lead_cols)
    };
    let fields = fit_fields(
        detail_fields(row, disp, now_unix, echo),
        cols.saturating_sub(lead_cols),
    );
    if fields.is_empty() {
        return None;
    }
    let mut segs = lead;
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            segs.push(sp(2));
        }
        segs.extend(field.segs);
    }
    Some(Line::Segs(segs))
}

/// The width-independent content predicate used by the heights pass.
pub(crate) fn detail_has_content(row: &SidebarRow, disp: &SidebarDisplay, echo: TitleEcho) -> bool {
    !detail_fields(row, disp, 0, echo).is_empty()
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
    use crate::sidebar::RowKind;
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
            stash: None,
        });
        row
    }

    fn builtin(field: DetailField) -> thegn_core::config::DetailEntry {
        thegn_core::config::DetailEntry::Builtin(field)
    }

    fn echo_all(branch: bool) -> TitleEcho {
        TitleEcho {
            branch,
            commits: true,
            pr: true,
            upstream: true,
        }
    }

    fn ids_of(row: &SidebarRow, disp: &SidebarDisplay, echo: TitleEcho) -> Vec<FieldId> {
        detail_fields(row, disp, 2_000, echo)
            .into_iter()
            .map(|f| f.id)
            .collect()
    }

    #[test]
    fn echo_truth_table() {
        let mut r = row();
        let d = SidebarDisplay::default();
        assert!(title_echo(&r, &d, "feature/retry-loop").branch);
        assert!(!title_echo(&r, &d, "feature").branch);
        let off = SidebarDisplay {
            show_commits: false,
            show_behind_base: false,
            show_pr_chip: false,
            show_ahead_behind: false,
            ..SidebarDisplay::default()
        };
        let e = title_echo(&r, &off, "x");
        assert!(!e.commits && !e.pr && !e.upstream);
        r.pr_number = Some(3);
        let on = SidebarDisplay {
            show_commits: true,
            show_pr_chip: true,
            ..off.clone()
        };
        let e = title_echo(&r, &on, "x");
        assert!(e.commits && e.pr && !e.upstream);
        // Empty branch falls back to label.
        r.branch = None;
        assert!(title_echo(&r, &d, "feature").branch);
        r.kind = RowKind::Folder;
        assert!(!title_echo(&r, &d, "feature").branch);
    }

    #[test]
    fn screenshot_row_has_no_detail_line() {
        let mut r = SidebarRow::base(RowKind::Worktree, 1, "tg/b158-config-dir-env", "repo");
        r.branch = Some("tg/b158-config-dir-env".into());
        r.branch_meta = Some(BranchMeta {
            ahead_base: Some(1),
            ..BranchMeta::default()
        });
        let d = SidebarDisplay::default();
        let echo = title_echo(&r, &d, "tg/b158-config-dir-env");
        assert!(!detail_has_content(&r, &d, echo));
    }

    #[test]
    fn home_keeps_its_branch_line() {
        let mut r = SidebarRow::base(RowKind::Worktree, 1, "home", "repo");
        r.branch = Some("main".into());
        let d = SidebarDisplay::default();
        let echo = title_echo(&r, &d, "home");
        assert!(!echo.branch);
        assert!(detail_has_content(&r, &d, echo));
    }

    #[test]
    fn a_window_title_keeps_the_branch() {
        let r = row();
        let d = SidebarDisplay::default();
        let echo = title_echo(&r, &d, "retry window");
        assert!(ids_of(&r, &d, echo).contains(&FieldId::Branch));
    }

    #[test]
    fn pr_number_moves_to_the_title_but_a_bare_count_stays() {
        let mut r = row();
        let d = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Pr)],
            ..SidebarDisplay::default()
        };
        r.pr_number = Some(42);
        let echo = title_echo(&r, &d, "x");
        assert!(ids_of(&r, &d, echo).is_empty());
        r.pr_number = None;
        r.pr_count = Some(2);
        let echo = title_echo(&r, &d, "x");
        assert_eq!(ids_of(&r, &d, echo), vec![FieldId::Pr]);
    }

    #[test]
    fn dirty_count_and_branch_stat_still_show() {
        let r = row();
        let d = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Dirty), builtin(DetailField::Commits)],
            ..SidebarDisplay::default()
        };
        let ids = ids_of(&r, &d, echo_all(true));
        assert_eq!(ids, vec![FieldId::Dirty]);
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
            detail_fields: vec![builtin(DetailField::Dirty)],
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
            detail_fields: vec![builtin(DetailField::Env)],
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
                builtin(DetailField::Upstream),
                builtin(DetailField::Stage),
                builtin(DetailField::Activity),
                builtin(DetailField::Env),
                builtin(DetailField::Sandbox),
                builtin(DetailField::Devcontainer),
                builtin(DetailField::Hibernated),
                builtin(DetailField::Mq),
                builtin(DetailField::Unread),
                builtin(DetailField::Disk),
                builtin(DetailField::Author),
            ],
            ..SidebarDisplay::default()
        };
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000, TitleEcho::default())
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
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000, TitleEcho::default())
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
            detail_fields: vec![builtin(DetailField::Env), builtin(DetailField::Branch)],
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
            detail_fields: vec![builtin(DetailField::Branch)],
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
            detail_fields: vec![builtin(DetailField::BranchStat)],
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

    fn detail_text(row: &SidebarRow, display: &SidebarDisplay, cols: usize) -> Option<String> {
        let line = compose_detail_line_at(row, display, 2_000, 0, cols)?;
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        Some(
            segs.iter()
                .map(|segment| segment.text.as_str())
                .collect::<String>(),
        )
    }

    fn with_stash(stash: Option<u32>) -> SidebarRow {
        let mut row = row();
        if let Some(meta) = row.branch_meta.as_mut() {
            meta.stash = stash;
        }
        row
    }

    #[test]
    fn stash_field_renders_count_and_hides_zero_and_unknown() {
        let display = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Stash)],
            ..SidebarDisplay::default()
        };
        assert_eq!(
            detail_text(&with_stash(Some(3)), &display, 80).as_deref(),
            Some("stash 3")
        );
        assert_eq!(
            detail_text(&with_stash(Some(META_COUNT_CAP)), &display, 80).as_deref(),
            Some("stash 999+")
        );
        assert_eq!(detail_text(&with_stash(Some(0)), &display, 80), None);
        assert_eq!(detail_text(&with_stash(None), &display, 80), None);
        let mut no_meta = row();
        no_meta.branch_meta = None;
        assert_eq!(detail_text(&no_meta, &display, 80), None);
    }

    #[test]
    fn stash_field_is_on_by_default_and_parses_from_config() {
        let row = with_stash(Some(2));
        let defaults = SidebarDisplay::default();
        assert!(
            defaults
                .detail_fields
                .contains(&builtin(DetailField::Stash))
        );
        assert!(
            detail_fields(&row, &defaults, 2_000, TitleEcho::default())
                .iter()
                .any(|field| field.id == FieldId::Stash)
        );

        let ui: thegn_core::config::UiConfig =
            toml::from_str(r#"sidebar_detail_fields = ["branch", "stash"]"#).unwrap();
        let display = SidebarDisplay {
            detail_fields: ui.detail_entries_resolved(),
            ..SidebarDisplay::default()
        };
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000, TitleEcho::default())
            .into_iter()
            .map(|field| field.id)
            .collect();
        assert_eq!(ids, vec![FieldId::Branch, FieldId::Stash]);
    }

    #[test]
    fn stash_drops_after_author_and_before_subject() {
        let ids = [
            FieldId::Branch,
            FieldId::Subject,
            FieldId::Stash,
            FieldId::Author,
        ];
        let fields = |ids: &[FieldId]| -> Vec<Field> {
            ids.iter()
                .map(|id| Field::new(*id, vec![seg(Tok::Slot(S::Dim), "1234567890")], 10))
                .collect()
        };
        let got = |budget: usize| -> Vec<FieldId> {
            fit_fields(fields(&ids), budget)
                .iter()
                .map(|field| field.id)
                .collect()
        };
        assert_eq!(
            got(36),
            vec![FieldId::Branch, FieldId::Subject, FieldId::Stash]
        );
        assert_eq!(got(24), vec![FieldId::Branch, FieldId::Subject]);
        assert_eq!(got(12), vec![FieldId::Branch]);
    }

    fn with_ci(state: Option<CiBadge>) -> SidebarRow {
        let mut row = row();
        row.ci_state = state;
        row
    }

    #[test]
    fn ci_field_renders_state_text_and_hides_unknown() {
        let display = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Ci)],
            ..SidebarDisplay::default()
        };
        for (state, text) in [
            (CiBadge::Pass, "ci pass"),
            (CiBadge::Fail, "ci fail"),
            (CiBadge::Running, "ci run"),
            (CiBadge::Pending, "ci pend"),
        ] {
            assert_eq!(
                detail_text(&with_ci(Some(state)), &display, 80).as_deref(),
                Some(text)
            );
        }
        assert_eq!(detail_text(&with_ci(None), &display, 80), None);
    }

    #[test]
    fn ci_fail_uses_the_error_role() {
        let fail = ci_field(CiBadge::Fail, FieldId::Ci);
        assert_eq!(fail.segs.len(), 1);
        assert_eq!(fail.segs[0].fg, plugin_role(RowRole::Bad));
        let pass = ci_field(CiBadge::Pass, FieldId::Ci);
        assert_eq!(pass.segs[0].fg, plugin_role(RowRole::Good));
    }

    #[test]
    fn ci_field_is_on_by_default_and_parses_from_config() {
        let row = with_ci(Some(CiBadge::Fail));
        let defaults = SidebarDisplay::default();
        assert!(defaults.detail_fields.contains(&builtin(DetailField::Ci)));
        assert!(
            detail_fields(&row, &defaults, 2_000, TitleEcho::default())
                .iter()
                .any(|field| field.id == FieldId::Ci)
        );

        let ui: thegn_core::config::UiConfig =
            toml::from_str(r#"sidebar_detail_fields = ["branch", "ci"]"#).unwrap();
        let display = SidebarDisplay {
            detail_fields: ui.detail_entries_resolved(),
            ..SidebarDisplay::default()
        };
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000, TitleEcho::default())
            .into_iter()
            .map(|field| field.id)
            .collect();
        assert_eq!(ids, vec![FieldId::Branch, FieldId::Ci]);
    }

    #[test]
    fn ci_drops_after_stash_and_before_subject() {
        let ids = [
            FieldId::Branch,
            FieldId::Subject,
            FieldId::Ci,
            FieldId::Stash,
        ];
        let fields = |ids: &[FieldId]| -> Vec<Field> {
            ids.iter()
                .map(|id| Field::new(*id, vec![seg(Tok::Slot(S::Dim), "1234567890")], 10))
                .collect()
        };
        let got = |budget: usize| -> Vec<FieldId> {
            fit_fields(fields(&ids), budget)
                .iter()
                .map(|field| field.id)
                .collect()
        };
        assert_eq!(
            got(36),
            vec![FieldId::Branch, FieldId::Subject, FieldId::Ci]
        );
        assert_eq!(got(24), vec![FieldId::Branch, FieldId::Subject]);
        assert_eq!(got(12), vec![FieldId::Branch]);
    }

    fn with_issue(issue: Option<(&str, usize)>) -> SidebarRow {
        let mut row = row();
        row.linked_issue = issue.map(|(id, count)| (id.to_string(), count));
        row
    }

    fn issue_display() -> SidebarDisplay {
        SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Issue)],
            ..SidebarDisplay::default()
        }
    }

    #[test]
    fn issue_field_renders_id_and_extra_count_and_hides_absent() {
        let display = issue_display();
        assert_eq!(
            detail_text(&with_issue(Some(("123", 1))), &display, 80).as_deref(),
            Some("#123")
        );
        assert_eq!(
            detail_text(&with_issue(Some(("ABC-9", 3))), &display, 80).as_deref(),
            Some("ABC-9 +2")
        );
        assert_eq!(
            detail_text(&with_issue(Some(("42", 2))), &display, 80).as_deref(),
            Some("#42 +1")
        );
        assert_eq!(detail_text(&with_issue(None), &display, 80), None);
        assert_eq!(
            detail_text(&with_issue(Some(("\u{1b}\u{7}", 1))), &display, 80),
            None
        );
    }

    #[test]
    fn issue_field_strips_control_characters() {
        let text = detail_text(
            &with_issue(Some(("AB\u{1b}[31mC-1\n", 1))),
            &issue_display(),
            80,
        )
        .unwrap();
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert_eq!(text, "AB[31mC-1");
    }

    #[test]
    fn issue_field_truncates_a_long_id() {
        let long = "X".repeat(200);
        let field = issue_field(&long, 1, FieldId::Issue).unwrap();
        assert_eq!(field.width, ISSUE_ID_MAX_CELLS);
        let digits = "9".repeat(200);
        let field = issue_field(&digits, 4, FieldId::Issue).unwrap();
        let text = field
            .segs
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(text.starts_with('#'), "{text:?}");
        assert!(text.ends_with(" +3"), "{text:?}");
        assert_eq!(field.width, ISSUE_ID_MAX_CELLS + "#".len() + " +3".len());
    }

    #[test]
    fn issue_field_is_on_by_default_and_parses_from_config() {
        let row = with_issue(Some(("ABC-9", 1)));
        let defaults = SidebarDisplay::default();
        assert!(
            defaults
                .detail_fields
                .contains(&builtin(DetailField::Issue))
        );
        assert!(
            detail_fields(&row, &defaults, 2_000, TitleEcho::default())
                .iter()
                .any(|field| field.id == FieldId::Issue)
        );

        let ui: thegn_core::config::UiConfig =
            toml::from_str(r#"sidebar_detail_fields = ["branch", "ci", "issue"]"#).unwrap();
        let display = SidebarDisplay {
            detail_fields: ui.detail_entries_resolved(),
            ..SidebarDisplay::default()
        };
        let ids: Vec<FieldId> = detail_fields(&row, &display, 2_000, TitleEcho::default())
            .into_iter()
            .map(|field| field.id)
            .collect();
        assert_eq!(ids, vec![FieldId::Branch, FieldId::Issue]);
    }

    #[test]
    fn issue_drops_after_ci_and_before_subject() {
        let ids = [
            FieldId::Branch,
            FieldId::Subject,
            FieldId::Issue,
            FieldId::Ci,
        ];
        let fields = |ids: &[FieldId]| -> Vec<Field> {
            ids.iter()
                .map(|id| Field::new(*id, vec![seg(Tok::Slot(S::Dim), "1234567890")], 10))
                .collect()
        };
        let got = |budget: usize| -> Vec<FieldId> {
            fit_fields(fields(&ids), budget)
                .iter()
                .map(|field| field.id)
                .collect()
        };
        assert_eq!(
            got(36),
            vec![FieldId::Branch, FieldId::Subject, FieldId::Issue]
        );
        assert_eq!(got(24), vec![FieldId::Branch, FieldId::Subject]);
        assert_eq!(got(12), vec![FieldId::Branch]);
    }

    #[test]
    fn detail_has_content_matches_compose_is_some() {
        let row = row();
        let display = SidebarDisplay::default();
        assert_eq!(
            detail_has_content(&row, &display, TitleEcho::default()),
            compose_detail_line_at(&row, &display, 2_000, 8, 24).is_some()
        );
    }

    #[test]
    fn detail_fields_order_is_respected() {
        let row = row();
        let display = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Subject), builtin(DetailField::Branch)],
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
            detail_fields: vec![builtin(DetailField::Pr)],
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
            detail_fields: vec![builtin(DetailField::Subject)],
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

    #[test]
    fn guided_detail_line_starts_with_the_lead_and_budgets_after_it() {
        let mut row = row();
        row.branch_meta.as_mut().unwrap().last_subject =
            Some("a rather long commit subject line".into());
        let display = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Subject)],
            ..SidebarDisplay::default()
        };
        let lead = vec![sp(4), seg(Tok::Slot(S::Ghost2), "| "), sp(2)];
        let Some(Line::Segs(segs)) = compose_detail_line_guided(
            &row,
            &display,
            2_000,
            TitleEcho::default(),
            lead.clone(),
            8,
            24,
        ) else {
            panic!("guided detail line is a segment line")
        };
        assert_eq!(&segs[..3], &lead[..]);
        assert!(seg_width(&segs) <= 24, "{segs:?}");
        // The fields only get `cols - lead_cols`.
        assert!(seg_width(&segs[3..]) <= 24 - 8, "{segs:?}");
    }

    #[test]
    fn guided_detail_line_falls_back_to_spaces_when_the_clamp_bites() {
        let row = row();
        let display = SidebarDisplay {
            detail_fields: vec![builtin(DetailField::Subject)],
            ..SidebarDisplay::default()
        };
        let lead = vec![sp(4), seg(Tok::Slot(S::Ghost2), "| "), sp(2)];
        let guided =
            compose_detail_line_guided(&row, &display, 2_000, TitleEcho::default(), lead, 8, 8);
        let plain = compose_detail_line_at(&row, &display, 2_000, 8, 8);
        let (Some(Line::Segs(g)), Some(Line::Segs(p))) = (guided, plain) else {
            panic!("both variants render a segment line")
        };
        assert_eq!(g, p);
        assert!(g.iter().all(|s| !s.text.contains('|')));
    }

    #[test]
    fn plugin_detail_field_renders_in_configured_position() {
        let mut row = row();
        row.plugin_fields.push(PluginFieldValue {
            plugin: "ci".into(),
            field: "check".into(),
            text: "ready".into(),
            role: RowRole::Good,
            slot: RowFieldSlot::Detail,
        });
        let display = SidebarDisplay {
            detail_fields: vec![
                DetailEntry::Plugin {
                    plugin: "ci".into(),
                    field: "check".into(),
                },
                builtin(DetailField::Branch),
            ],
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
        assert!(text.find("ready").unwrap() < text.find("feature/retry").unwrap());
    }

    #[test]
    fn plugin_detail_fields_drop_before_subject_at_narrow_width() {
        let mut row = row();
        row.plugin_fields.push(PluginFieldValue {
            plugin: "ci".into(),
            field: "check".into(),
            text: "ready".into(),
            role: RowRole::Dim,
            slot: RowFieldSlot::Detail,
        });
        let display = SidebarDisplay {
            detail_fields: vec![
                DetailEntry::Plugin {
                    plugin: "ci".into(),
                    field: "check".into(),
                },
                builtin(DetailField::Subject),
                builtin(DetailField::Branch),
            ],
            ..SidebarDisplay::default()
        };
        let fitted = fit_fields(
            detail_fields(&row, &display, 2_000, TitleEcho::default()),
            20,
        );
        assert!(!fitted.iter().any(|field| field.id == FieldId::Plugin));
        assert!(fitted.iter().any(|field| field.id == FieldId::Branch));
    }

    #[test]
    fn plugin_detail_defensively_sanitises_text_and_absent_values_do_not_shift() {
        let mut row = row();
        row.plugin_fields.push(PluginFieldValue {
            plugin: "ci".into(),
            field: "check".into(),
            text: "ok\u{1b}[31m\nnow".into(),
            role: RowRole::Unknown,
            slot: RowFieldSlot::Detail,
        });
        let display = SidebarDisplay {
            detail_fields: vec![
                DetailEntry::Plugin {
                    plugin: "missing".into(),
                    field: "badge".into(),
                },
                DetailEntry::Plugin {
                    plugin: "ci".into(),
                    field: "check".into(),
                },
                builtin(DetailField::Branch),
            ],
            ..SidebarDisplay::default()
        };
        let line = compose_detail_line_at(&row, &display, 2_000, 0, 80).unwrap();
        let Line::Segs(segs) = line else {
            panic!("detail line is a segment line")
        };
        assert!(
            segs.iter()
                .all(|segment| !segment.text.chars().any(char::is_control))
        );
        assert!(segs.iter().any(|segment| segment.text.contains("ok")));
        assert!(segs.iter().any(|segment| segment.fg == Tok::Slot(S::Dim)));
        assert!(
            segs.iter()
                .any(|segment| segment.text.contains("feature/retry"))
        );
    }
}
