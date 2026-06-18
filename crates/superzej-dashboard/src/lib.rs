use std::sync::{Arc, Mutex};

#[derive(Clone, Default, Debug, PartialEq)]
pub struct SystemSummary {
    pub os: String,
    pub uptime_secs: u64,
    pub mem_used_gib: f64,
    pub mem_total_gib: f64,
    pub cpu_count: usize,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct ResourceSummary {
    pub cpu_pct: f64,
    pub load_one: f64,
    pub process_count: usize,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct HackerNewsStory {
    pub title: String,
    pub url: String,
    pub points: u64,
    pub comments: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardOptions {
    pub interval_secs: u64,
    pub hacker_news: bool,
    pub hacker_news_limit: usize,
}

impl Default for DashboardOptions {
    fn default() -> Self {
        DashboardOptions {
            interval_secs: 4,
            hacker_news: true,
            hacker_news_limit: 5,
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct RepoSummary {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct WorkspaceSummary {
    pub name: String,
    pub path: String,
    pub kind: String,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct MetricTargetSummary {
    pub name: String,
    pub url: String,
    pub metric_count: usize,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct DashboardData {
    pub generated_at: u64,
    pub interval_secs: u64,
    pub system: SystemSummary,
    pub resources: ResourceSummary,
    pub hacker_news: Vec<HackerNewsStory>,
    pub recent_repos: Vec<RepoSummary>,
    pub workspaces: Vec<WorkspaceSummary>,
    pub metric_targets: Vec<MetricTargetSummary>,
}

use sz_kit::ratatui::buffer::Buffer;
use sz_kit::ratatui::layout::{Constraint, Direction, Layout, Rect};
use sz_kit::ratatui::style::Style;
use sz_kit::ratatui::text::{Line, Span};
use sz_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget};
use sz_kit::{AppTile, ChangeHook, InputEvent, InputResult, Key, Theme};

#[derive(serde::Deserialize)]
struct HnItem {
    title: Option<String>,
    url: Option<String>,
    score: Option<u64>,
    descendants: Option<u64>,
}

fn fetch_hacker_news(limit: usize) -> Vec<HackerNewsStory> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .user_agent("superzej-dashboard/0.1")
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let ids: Vec<u64> = match client
        .get("https://hacker-news.firebaseio.com/v0/topstories.json")
        .send()
        .and_then(|resp| resp.error_for_status())
        .and_then(|resp| resp.json())
    {
        Ok(ids) => ids,
        Err(_) => return Vec::new(),
    };

    ids.into_iter()
        .take(limit.clamp(1, 20))
        .filter_map(|id| {
            let item: HnItem = client
                .get(format!(
                    "https://hacker-news.firebaseio.com/v0/item/{id}.json"
                ))
                .send()
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .ok()?;
            let title = item.title?;
            Some(HackerNewsStory {
                title,
                url: item
                    .url
                    .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={id}")),
                points: item.score.unwrap_or(0),
                comments: item.descendants.unwrap_or(0),
            })
        })
        .collect()
}

pub struct DashboardUi {
    theme: Theme,
    data: Arc<Mutex<DashboardData>>,
    on_change: Option<ChangeHook>,
}

impl DashboardUi {
    pub fn new(
        _rt: tokio::runtime::Handle,
        on_change: Option<ChangeHook>,
        theme: Theme,
        options: DashboardOptions,
    ) -> Self {
        let data = Arc::new(Mutex::new(DashboardData::default()));

        let data_clone = data.clone();
        let hook_clone = on_change.clone();
        std::thread::Builder::new()
            .name("superzej-dashboard".into())
            .spawn(move || {
                let mut sys = sysinfo::System::new_all();
                let mut hn_stories = Vec::new();
                let mut next_hn_refresh = std::time::Instant::now();
                loop {
                    sys.refresh_all();
                    let mem_gb = sys.used_memory() as f64 / 1_073_741_824.0;
                    let total_gb = sys.total_memory() as f64 / 1_073_741_824.0;
                    let cpu_count = sys.cpus().len();
                    let cpu_pct = if cpu_count == 0 {
                        0.0
                    } else {
                        sys.cpus()
                            .iter()
                            .map(|cpu| cpu.cpu_usage() as f64)
                            .sum::<f64>()
                            / cpu_count as f64
                    };
                    let load = sysinfo::System::load_average();
                    let process_count = sys.processes().len();

                    if options.hacker_news && std::time::Instant::now() >= next_hn_refresh {
                        let fetched = fetch_hacker_news(options.hacker_news_limit);
                        if !fetched.is_empty() {
                            hn_stories = fetched;
                        }
                        next_hn_refresh =
                            std::time::Instant::now() + std::time::Duration::from_secs(600);
                    }

                    let mut repos = vec![];
                    let mut wss = vec![];
                    if let Ok(db) = superzej_core::db::Db::open() {
                        if let Ok(recent) = db.recent_repos(15) {
                            for path in recent {
                                let name = std::path::Path::new(&path)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned();
                                repos.push(RepoSummary { name, path });
                            }
                        }
                        if let Ok(ws_rows) = db.workspaces() {
                            wss = ws_rows
                                .into_iter()
                                .map(|w| WorkspaceSummary {
                                    name: w.name,
                                    path: w.repo_path,
                                    kind: w.kind,
                                })
                                .collect();
                        }
                    }

                    let generated_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();

                    {
                        let mut d = data_clone.lock().unwrap();
                        d.generated_at = generated_at;
                        d.interval_secs = options.interval_secs.max(1);
                        d.system = SystemSummary {
                            os: sysinfo::System::long_os_version().unwrap_or_default(),
                            uptime_secs: sysinfo::System::uptime(),
                            mem_used_gib: mem_gb,
                            mem_total_gib: total_gb,
                            cpu_count,
                        };
                        d.resources = ResourceSummary {
                            cpu_pct,
                            load_one: load.one,
                            process_count,
                        };
                        d.hacker_news = hn_stories.clone();
                        d.recent_repos = repos;
                        d.workspaces = wss;
                        // Note: full metrics via channel would require the host to push them down,
                        // we'll leave target summary empty in this tile component itself, or just static.
                    }

                    if let Some(hook) = &hook_clone {
                        hook();
                    }

                    std::thread::sleep(std::time::Duration::from_secs(
                        options.interval_secs.max(1),
                    ));
                }
            })
            .ok();

        Self {
            theme,
            data,
            on_change,
        }
    }

    /// For tests.
    pub fn with_data(data: DashboardData, theme: Theme) -> Self {
        Self {
            theme,
            data: Arc::new(Mutex::new(data)),
            on_change: None,
        }
    }

    fn render_system_info(&self, data: &DashboardData, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" System ")
            .border_style(Style::default().fg(self.theme.border.into()));

        let text = vec![
            Line::from(vec![
                Span::styled("OS: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(&data.system.os, Style::default().fg(self.theme.text.into())),
            ]),
            Line::from(vec![
                Span::styled("Uptime: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!("{}s", data.system.uptime_secs),
                    Style::default().fg(self.theme.text.into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("Memory: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!(
                        "{:.2} GB / {:.2} GB",
                        data.system.mem_used_gib, data.system.mem_total_gib
                    ),
                    Style::default().fg(self.theme.text.into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("CPUs: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!("{}", data.system.cpu_count),
                    Style::default().fg(self.theme.text.into()),
                ),
            ]),
        ];

        Paragraph::new(text).block(block).render(area, buf);
    }

    fn render_resources(&self, data: &DashboardData, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Resources ")
            .border_style(Style::default().fg(self.theme.border.into()));
        let mem_pct = if data.system.mem_total_gib > 0.0 {
            (data.system.mem_used_gib / data.system.mem_total_gib) * 100.0
        } else {
            0.0
        };
        let text = vec![
            Line::from(vec![
                Span::styled("CPU: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!("{:.1}%", data.resources.cpu_pct),
                    Style::default().fg(self.theme.green.into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("Mem: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!(
                        "{mem_pct:.1}% ({:.1}/{:.1}G)",
                        data.system.mem_used_gib, data.system.mem_total_gib
                    ),
                    Style::default().fg(self.theme.teal.into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("Load: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!("{:.2}", data.resources.load_one),
                    Style::default().fg(self.theme.amber.into()),
                ),
            ]),
            Line::from(vec![
                Span::styled("Processes: ", Style::default().fg(self.theme.dim.into())),
                Span::styled(
                    format!("{}", data.resources.process_count),
                    Style::default().fg(self.theme.text.into()),
                ),
            ]),
        ];
        Paragraph::new(text).block(block).render(area, buf);
    }

    fn render_hacker_news(&self, data: &DashboardData, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Hacker News ")
            .border_style(Style::default().fg(self.theme.border.into()));
        let mut lines = Vec::new();
        if data.hacker_news.is_empty() {
            lines.push(Line::from(Span::styled(
                "No HN stories yet (offline or loading).",
                Style::default().fg(self.theme.faint.into()),
            )));
        } else {
            for (idx, story) in data.hacker_news.iter().take(8).enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>2}. ", idx + 1),
                        Style::default().fg(self.theme.dim.into()),
                    ),
                    Span::styled(&story.title, Style::default().fg(self.theme.text.into())),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default().fg(self.theme.dim.into())),
                    Span::styled(
                        format!("{} pts · {} comments", story.points, story.comments),
                        Style::default().fg(self.theme.amber.into()),
                    ),
                ]));
            }
        }
        Paragraph::new(lines).block(block).render(area, buf);
    }

    fn render_repos(&self, data: &DashboardData, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Recent Repos ")
            .border_style(Style::default().fg(self.theme.border.into()));

        let mut lines = vec![];
        if data.recent_repos.is_empty() {
            lines.push(Line::from(Span::styled(
                "No recent repos.",
                Style::default().fg(self.theme.faint.into()),
            )));
        } else {
            for repo in data.recent_repos.iter().take(10) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<15} ", repo.name),
                        Style::default().fg(self.theme.accent.into()),
                    ),
                    Span::styled(&repo.path, Style::default().fg(self.theme.dim.into())),
                ]));
            }
        }

        Paragraph::new(lines).block(block).render(area, buf);
    }

    fn render_workspaces(&self, data: &DashboardData, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Workspaces ")
            .border_style(Style::default().fg(self.theme.border.into()));

        let mut lines = vec![];
        if data.workspaces.is_empty() {
            lines.push(Line::from(Span::styled(
                "No open workspaces.",
                Style::default().fg(self.theme.faint.into()),
            )));
        } else {
            for ws in data.workspaces.iter().take(15) {
                let kind_color = if ws.kind == "repo" {
                    self.theme.green
                } else {
                    self.theme.blue
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("[{:<4}] ", ws.kind),
                        Style::default().fg(kind_color.into()),
                    ),
                    Span::styled(
                        format!("{:<15} ", ws.name),
                        Style::default().fg(self.theme.text.into()),
                    ),
                    Span::styled(&ws.path, Style::default().fg(self.theme.dim.into())),
                ]));
            }
        }

        Paragraph::new(lines).block(block).render(area, buf);
    }

    fn render_metrics(&self, data: &DashboardData, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Metrics ")
            .border_style(Style::default().fg(self.theme.border.into()));

        let mut lines = vec![];
        if data.metric_targets.is_empty() {
            lines.push(Line::from(Span::styled(
                "No metric targets configured.",
                Style::default().fg(self.theme.faint.into()),
            )));
        } else {
            for t in &data.metric_targets {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<15} ", t.name),
                        Style::default().fg(self.theme.teal.into()),
                    ),
                    Span::styled(
                        format!("{} metrics", t.metric_count),
                        Style::default().fg(self.theme.dim.into()),
                    ),
                ]));
            }
        }

        Paragraph::new(lines).block(block).render(area, buf);
    }
}

impl AppTile for DashboardUi {
    fn id(&self) -> &'static str {
        "dashboard"
    }

    fn title(&self) -> String {
        "dashboard".into()
    }

    fn pump(&mut self) -> bool {
        false
    }

    fn wants_redraw(&self) -> bool {
        true
    }

    fn handle_input(&mut self, ev: InputEvent) -> InputResult {
        if let InputEvent::Key { key, .. } = ev {
            match key {
                Key::Char('q') | Key::Escape => InputResult::Exit,
                Key::Char('r') => {
                    // Manual refresh request if we want to kick it
                    if let Some(hook) = &self.on_change {
                        hook();
                    }
                    InputResult::Consumed
                }
                _ => InputResult::Ignored,
            }
        } else {
            InputResult::Ignored
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let data = self.data.lock().unwrap().clone();

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)].as_ref())
            .split(area);

        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(6),
                    Constraint::Length(7),
                    Constraint::Min(0),
                ]
                .as_ref(),
            )
            .split(chunks[0]);

        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Percentage(38),
                    Constraint::Percentage(24),
                    Constraint::Percentage(24),
                    Constraint::Min(4),
                ]
                .as_ref(),
            )
            .split(chunks[1]);

        self.render_system_info(&data, left_chunks[0], buf);
        self.render_resources(&data, left_chunks[1], buf);
        self.render_metrics(&data, left_chunks[2], buf);

        self.render_hacker_news(&data, right_chunks[0], buf);
        self.render_repos(&data, right_chunks[1], buf);
        self.render_workspaces(&data, right_chunks[2], buf);

        let header_block = Block::default()
            .borders(Borders::ALL)
            .title(" Superzej Dashboard ")
            .border_style(Style::default().fg(self.theme.border.into()));

        let help = Paragraph::new(format!(
            "Last updated: {}  |  Refresh interval: {}s  |  Press 'q' or 'Esc' or 'Alt-d' to close",
            data.generated_at, data.interval_secs
        ))
        .style(Style::default().fg(self.theme.dim.into()))
        .block(header_block);

        help.render(right_chunks[3], buf);
    }
}

pub fn run_standalone() -> anyhow::Result<()> {
    #[cfg(feature = "standalone")]
    {
        sz_kit::standalone::run(|hook| {
            let rt = tokio::runtime::Runtime::new()?;
            Ok(Box::new(DashboardUi::new(
                rt.handle().clone(),
                Some(hook),
                Theme::prism(),
                DashboardOptions::default(),
            )))
        })
    }
    #[cfg(not(feature = "standalone"))]
    {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sz_kit::ratatui::buffer::Buffer;
    use sz_kit::ratatui::layout::Rect;
    use sz_kit::{InputEvent, InputResult, Key, Theme};

    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn dashboard_renders_wtf_style_sections_from_snapshot() {
        let data = DashboardData {
            generated_at: 123,
            interval_secs: 7,
            system: SystemSummary {
                os: "TestOS".into(),
                uptime_secs: 65,
                mem_used_gib: 1.5,
                mem_total_gib: 8.0,
                cpu_count: 4,
            },
            resources: ResourceSummary::default(),
            hacker_news: Vec::new(),
            recent_repos: vec![RepoSummary {
                name: "superzej".into(),
                path: "/code/superzej".into(),
            }],
            workspaces: vec![WorkspaceSummary {
                name: "superzej".into(),
                path: "/code/superzej".into(),
                kind: "repo".into(),
            }],
            metric_targets: vec![MetricTargetSummary {
                name: "model-proxy".into(),
                url: "http://127.0.0.1:9091/metrics".into(),
                metric_count: 2,
            }],
        };
        let mut ui = DashboardUi::with_data(data, Theme::prism());
        let area = Rect::new(0, 0, 100, 28);
        let mut buf = Buffer::empty(area);

        ui.render(area, &mut buf);
        let text = buffer_text(&buf);

        assert!(text.contains("Superzej Dashboard"), "{text}");
        assert!(text.contains("System"), "{text}");
        assert!(text.contains("Recent Repos"), "{text}");
        assert!(text.contains("Workspaces"), "{text}");
        assert!(text.contains("Metrics"), "{text}");
        assert!(text.contains("TestOS"), "{text}");
        assert!(text.contains("superzej"), "{text}");
        assert!(text.contains("model-proxy"), "{text}");
    }

    #[test]
    fn dashboard_renders_hacker_news_and_resource_widgets_from_snapshot() {
        let data = DashboardData {
            generated_at: 123,
            interval_secs: 7,
            system: SystemSummary {
                os: "TestOS".into(),
                uptime_secs: 65,
                mem_used_gib: 1.5,
                mem_total_gib: 8.0,
                cpu_count: 4,
            },
            resources: ResourceSummary {
                cpu_pct: 42.5,
                load_one: 1.25,
                process_count: 321,
            },
            hacker_news: vec![HackerNewsStory {
                title: "Show HN: Superzej Dashboard".into(),
                url: "https://news.ycombinator.com/item?id=1".into(),
                points: 123,
                comments: 45,
            }],
            recent_repos: vec![RepoSummary {
                name: "superzej".into(),
                path: "/code/superzej".into(),
            }],
            workspaces: vec![WorkspaceSummary {
                name: "superzej".into(),
                path: "/code/superzej".into(),
                kind: "repo".into(),
            }],
            metric_targets: vec![MetricTargetSummary {
                name: "model-proxy".into(),
                url: "http://127.0.0.1:9091/metrics".into(),
                metric_count: 2,
            }],
        };
        let mut ui = DashboardUi::with_data(data, Theme::prism());
        let area = Rect::new(0, 0, 120, 34);
        let mut buf = Buffer::empty(area);

        ui.render(area, &mut buf);
        let text = buffer_text(&buf);

        assert!(text.contains("Resources"), "{text}");
        assert!(text.contains("CPU: 42.5%"), "{text}");
        assert!(text.contains("Load: 1.25"), "{text}");
        assert!(text.contains("Hacker News"), "{text}");
        assert!(text.contains("Show HN: Superzej Dashboard"), "{text}");
        assert!(text.contains("123 pts"), "{text}");
    }

    #[test]
    fn dashboard_input_q_or_escape_exits_and_r_refreshes() {
        let mut ui = DashboardUi::with_data(DashboardData::default(), Theme::prism());

        assert_eq!(
            ui.handle_input(InputEvent::key(Key::Char('q'))),
            InputResult::Exit
        );
        assert_eq!(
            ui.handle_input(InputEvent::key(Key::Escape)),
            InputResult::Exit
        );
        assert_eq!(
            ui.handle_input(InputEvent::key(Key::Char('r'))),
            InputResult::Consumed
        );
    }
}
