use super::*;
use crate::apps::{ActiveApp, AppHost, AppSlot, SlotState};
use crate::center::{Dir, Grow};
use crate::chrome::FrameModel;
use crate::focus::{FocusState, Zone};
use crate::session::{Session, WorktreeGroup};

struct TestTile;

impl tg_kit::AppTile for TestTile {
    fn id(&self) -> &'static str {
        "test"
    }

    fn title(&self) -> String {
        "test".into()
    }

    fn pump(&mut self) -> bool {
        false
    }

    fn wants_redraw(&self) -> bool {
        false
    }

    fn handle_input(&mut self, _: tg_kit::InputEvent) -> tg_kit::InputResult {
        tg_kit::InputResult::Ignored
    }

    fn render(
        &mut self,
        _: tg_kit::ratatui::layout::Rect,
        _: &mut tg_kit::ratatui::buffer::Buffer,
    ) {
    }
}

struct Fixture {
    app: AppHost,
    panes: crate::panes::Panes,
    session: Session,
    focus: FocusState,
    chrome: crate::layout::ChromeLayout,
    model: FrameModel,
    help: Option<crate::help::HelpOverlay>,
    drawer: Option<u32>,
    down: bool,
    selecting: bool,
    selection: Option<(u32, crate::copymode::Selection)>,
    dirty: bool,
    capture: bool,
}

impl Fixture {
    fn new() -> Self {
        crate::caret::begin_frame();
        let mut group = WorktreeGroup::terminal("test");
        let tab = &mut group.tabs[0];
        tab.center = crate::center::CenterTree::single(1);
        tab.center.split(1, Dir::Row, 2);
        tab.focused_pane = 1;
        tab.grow = Grow::Maximized;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut panes = crate::panes::Panes::new(tx);
        panes.insert_test_pane(1);
        panes.insert_test_pane(2);
        let mut slot = AppSlot::new("test", "test");
        slot.state = SlotState::Running(Box::new(TestTile));
        Self {
            app: AppHost::new(vec![slot]),
            panes,
            session: Session {
                worktrees: vec![group],
                ..Session::default()
            },
            focus: FocusState {
                zone: Zone::Sidebar,
                locked: false,
            },
            chrome: crate::layout::compute(100, 40, false, false),
            model: FrameModel::default(),
            help: None,
            drawer: None,
            down: false,
            selecting: true,
            selection: Some((1, crate::copymode::Selection::new((0, 0)))),
            dirty: false,
            capture: false,
        }
    }

    fn bar(&self) -> Rect {
        let bars = crate::handlers::pane_zoom::displayed_tree(&self.session)
            .stack_bars(self.chrome.center);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].0, 2);
        bars[0].1
    }

    fn press_bar(&mut self) -> MousePre {
        let bar = self.bar();
        self.press_at(bar.x, bar.y)
    }

    fn press_at(&mut self, x: usize, y: usize) -> MousePre {
        let m = MouseEvent {
            x: (x + 1) as u16,
            y: (y + 1) as u16,
            mouse_buttons: MouseButtons::LEFT,
            modifiers: Modifiers::NONE,
        };
        pre_dispatch(
            true,
            &mut None,
            &mut None,
            &mut None,
            &mut self.help,
            &m,
            x,
            y,
            true,
            100,
            40,
            &self.chrome,
            &self.model,
            &mut self.app,
            self.drawer,
            &mut self.panes,
            &mut self.focus,
            &mut self.session,
            &mut None,
            &mut self.down,
            &mut self.selecting,
            &mut self.selection,
            &mut self.dirty,
            self.capture,
        )
    }

    fn assert_no_activation(&self) {
        assert_eq!(self.session.active_tab().unwrap().focused_pane, 1);
        assert_eq!(self.session.active_tab().unwrap().grow, Grow::Maximized);
        assert_eq!(self.focus.zone, Zone::Sidebar);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        crate::caret::begin_frame();
    }
}

#[test]
fn pre_dispatch_visible_stack_expands_but_running_app_does_not() {
    let mut f = Fixture::new();
    f.app.active = ActiveApp::Tile(0);
    assert!(f.app.active_tile_mut().is_some(), "exercise real takeover");
    assert!(matches!(f.press_bar(), MousePre::Fall(None, _)));
    f.assert_no_activation();
    assert!(!f.down);
    assert!(f.selecting && f.selection.is_some());

    // Same geometry and pane state, now actually displayed by the work tab.
    f.app.active = ActiveApp::Work;
    let MousePre::StackBar(id) = f.press_bar() else {
        panic!("visible collapsed bar must activate");
    };
    assert_eq!(id, 2);
    assert!(f.down);
    assert!(!f.selecting && f.selection.is_none());
    crate::handlers::pane_zoom::activate_stack_member(&mut f.session, &mut f.focus, id);
    assert_eq!(f.session.active_tab().unwrap().focused_pane, 2);
    assert_eq!(f.session.active_tab().unwrap().grow, Grow::Maximized);
    assert_eq!(f.focus.zone, Zone::Center);
}

#[test]
fn pre_dispatch_splash_has_no_clickable_stack_bars() {
    let mut f = Fixture::new();
    // A grown tree has one visible frame even though it has two stack members.
    f.model.load_steps = vec![crate::chrome::LoadStep::active("shell")];
    assert!(matches!(f.press_bar(), MousePre::Fall(None, _)));
    f.assert_no_activation();
    assert!(!f.down);

    f.model.load_steps.clear();
    f.panes.table.clear();
    assert!(matches!(f.press_bar(), MousePre::Fall(None, _)));
    f.assert_no_activation();
    assert!(!f.down);

    f.panes.insert_test_pane(1);
    assert!(matches!(f.press_bar(), MousePre::StackBar(2)));
}

#[test]
fn pre_dispatch_covered_bar_ignores_cursor_claim_and_resets_on_full_frame() {
    let mut f = Fixture::new();
    let bar = f.bar();
    crate::caret::cover(bar);
    crate::caret::claim(bar.x, bar.y);
    assert!(matches!(f.press_bar(), MousePre::Fall(None, _)));
    f.assert_no_activation();
    assert!(!f.down);

    // A full compose retires closed overlays; an unrelated cover must not
    // disable a bar that remains visible (e.g. a toast elsewhere).
    crate::caret::begin_frame();
    crate::caret::cover(f.chrome.masthead);
    assert!(matches!(f.press_bar(), MousePre::StackBar(2)));
}

#[test]
fn pre_dispatch_modal_help_still_consumes_before_stack_dispatch() {
    let mut f = Fixture::new();
    let (reg, errors) = crate::help::pages::build_registry(&thegn_core::config::Config::default());
    assert!(errors.is_empty());
    f.help = Some(crate::help::HelpOverlay::new(
        std::sync::Arc::new(reg),
        "index".into(),
        "F1".into(),
    ));
    assert!(matches!(f.press_bar(), MousePre::Consumed));
    f.assert_no_activation();
    assert!(f.down);
    assert!(!f.selecting && f.selection.is_none());
}

#[test]
fn pre_dispatch_stack_respects_pointer_capture_held_press_and_drawer() {
    let mut f = Fixture::new();
    f.capture = true;
    assert!(matches!(f.press_bar(), MousePre::Fall(None, _)));
    f.assert_no_activation();
    assert!(!f.down);

    f.capture = false;
    f.down = true;
    assert!(matches!(f.press_bar(), MousePre::Fall(None, _)));
    f.assert_no_activation();

    f.down = false;
    f.drawer = Some(99);
    let bar = f.bar();
    f.chrome.drawer = Some(bar);
    assert!(matches!(f.press_bar(), MousePre::Fall(Some((99, r)), _) if r == bar));
    f.assert_no_activation();
    assert!(!f.down);
}

#[test]
fn pre_dispatch_does_not_activate_an_unpaintable_one_column_bar() {
    let mut f = Fixture::new();
    f.chrome.center.cols = 1;
    let center = f.chrome.center;
    let response = f.press_at(center.x, center.y + center.rows - 1);
    assert!(matches!(response, MousePre::Fall(None, _)));
    f.assert_no_activation();
    assert!(!f.down);

    f.chrome.center.cols = 2;
    assert!(matches!(f.press_bar(), MousePre::StackBar(2)));
}
