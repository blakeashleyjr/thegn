//! `ThemeLoadError` implements `std::error::Error + Send + Sync + 'static`
//! (Rust API Guidelines C-GOOD-ERR) and renders a readable message.

use std::error::Error;

use tg_kit::theme::ThemeLoadError;

fn assert_error<E: Error + Send + Sync + 'static>() {}

#[test]
fn theme_load_error_implements_std_error() {
    assert_error::<ThemeLoadError>();
}

#[test]
fn theme_load_error_messages_are_descriptive() {
    assert_eq!(
        ThemeLoadError::Oversize.to_string(),
        "theme config is too large"
    );
    assert_eq!(
        ThemeLoadError::Malformed("bad toml".into()).to_string(),
        "theme config is malformed: bad toml"
    );
}
