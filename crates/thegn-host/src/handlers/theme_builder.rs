//! Loop-side event routing for the pure theme builder and its worker provider.

use termwiz::input::{KeyCode, Modifiers, MouseEvent};
use termwiz::surface::Surface;

use crate::compositor::Rect;
use crate::theme_builder::{BuilderEvent, ThemeBuilder};
use crate::theme_store::{ThemeStore, ThemeStoreResult};

pub(crate) struct DrainOutcome {
    pub close: bool,
    pub dirty: bool,
}

pub(crate) fn open(
    cfg: &thegn_core::config::Config,
    users: &[thegn_core::theme_user::UserTheme],
    store: &ThemeStore,
) -> ThemeBuilder {
    store.scan();
    ThemeBuilder::open(cfg, users)
}

pub(crate) fn key(
    builder: &mut ThemeBuilder,
    store: &ThemeStore,
    key: &KeyCode,
    mods: Modifiers,
) -> bool {
    match builder.handle_key(key, mods) {
        BuilderEvent::None => true,
        BuilderEvent::Close => {
            crate::chrome::set_palette(builder.cancel_palette());
            false
        }
        BuilderEvent::Import(path) => {
            store.import(path);
            true
        }
        BuilderEvent::Save(theme) => {
            store.save(theme);
            true
        }
        BuilderEvent::Apply {
            preset,
            theme,
            persist_theme,
        } => {
            store.apply(preset, theme, persist_theme);
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn mouse(
    builder: &mut ThemeBuilder,
    store: &ThemeStore,
    event: &MouseEvent,
    mx: usize,
    my: usize,
    screen: Rect,
    dismiss_outside: bool,
    mouse_left_down: &mut bool,
) -> bool {
    match builder.handle_mouse(event, mx, my, screen, dismiss_outside, mouse_left_down) {
        BuilderEvent::None => true,
        BuilderEvent::Close => {
            crate::chrome::set_palette(builder.cancel_palette());
            false
        }
        BuilderEvent::Import(path) => {
            store.import(path);
            true
        }
        BuilderEvent::Save(theme) => {
            store.save(theme);
            true
        }
        BuilderEvent::Apply {
            preset,
            theme,
            persist_theme,
        } => {
            store.apply(preset, theme, persist_theme);
            true
        }
    }
}

pub(crate) fn drain(
    builder: &mut ThemeBuilder,
    store: &mut ThemeStore,
    cfg: &thegn_core::config::Config,
    users: &mut Vec<thegn_core::theme_user::UserTheme>,
) -> DrainOutcome {
    let mut outcome = DrainOutcome {
        close: false,
        dirty: false,
    };
    while let Some(result) = store.try_recv() {
        outcome.dirty = true;
        match result {
            ThemeStoreResult::Catalog { themes, warnings } => {
                *users = themes;
                builder.set_catalog(cfg, users);
                if let Some(warning) = warnings.first() {
                    builder.set_status(warning.clone());
                }
            }
            ThemeStoreResult::Imported(result) => builder.import_completed(result),
            ThemeStoreResult::Saved(result) => {
                builder.saved(result);
                store.scan();
            }
            ThemeStoreResult::Applied(result) => {
                builder.store_completed(result);
                if builder.status().is_none() {
                    outcome.close = true;
                }
            }
        }
        crate::chrome::set_palette(builder.candidate().clone());
    }
    outcome
}

pub(crate) fn drain_catalog(
    store: &mut ThemeStore,
    users: &mut Vec<thegn_core::theme_user::UserTheme>,
) {
    while let Some(result) = store.try_recv() {
        if let ThemeStoreResult::Catalog { themes, .. } = result {
            *users = themes;
        }
    }
}

pub(crate) fn render(builder: &ThemeBuilder, surface: &mut Surface, screen: Rect) {
    builder.render(surface, screen);
}
