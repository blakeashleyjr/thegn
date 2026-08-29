//! Git-specific configuration enums kept beside the main config model.

use crate::config::{config_enum, config_warn};

config_enum! {
    /// Submodule lifecycle policy. `auto` initializes only repositories that
    /// have a root-level `.gitmodules`; `off` performs no recursive lifecycle
    /// work and only permits cheap gitlink classification.
    pub enum SubmoduleMode: "submodule mode" {
        Auto = "auto", Off = "off",
    } default = Auto;
}

#[cfg(test)]
mod tests {
    use super::SubmoduleMode;

    #[test]
    fn submodule_mode_defaults_to_auto_and_round_trips() {
        assert_eq!(SubmoduleMode::default(), SubmoduleMode::Auto);
        assert_eq!(
            SubmoduleMode::from_str_validated("off").unwrap(),
            SubmoduleMode::Off
        );
        assert_eq!(SubmoduleMode::Auto.to_string(), "auto");
        assert!(SubmoduleMode::from_str_validated("prompt").is_err());
    }
}
