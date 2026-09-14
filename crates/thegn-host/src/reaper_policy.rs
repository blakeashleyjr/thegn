//! Admission before provider access or ledger retirement. The legacy ledger
//! has no environment/account binding, so one provider kind must have one
//! unambiguous account reference and lifetime policy for automatic cleanup.
use thegn_core::config::EnvProviderConfig;

pub(crate) fn with_unambiguous<'a, T>(
    envs: impl IntoIterator<Item = &'a EnvProviderConfig>,
    reconcile: impl FnOnce(&'a EnvProviderConfig) -> T,
) -> Result<Option<T>, &'static str> {
    let mut envs = envs.into_iter();
    let Some(first) = envs.next() else {
        return Ok(None);
    };
    if envs.any(|other| {
        other.provider.trim() != first.provider.trim()
            || other.api_base != first.api_base
            || other.api_key_env.trim() != first.api_key_env.trim()
            || other.max_lifetime_secs != first.max_lifetime_secs
    }) {
        return Err(
            "ambiguous environment account/lifetime policy; bind ownership or align configuration before automatic cleanup",
        );
    }
    Ok(Some(reconcile(first)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguity_precedes_inventory_deletion_and_ledger_retirement() {
        let first = EnvProviderConfig {
            provider: "fly".into(),
            api_key_env: "ACCOUNT_A".into(),
            max_lifetime_secs: 60,
            ..Default::default()
        };
        for field in ["lifetime", "credentials", "endpoint"] {
            let mut other = first.clone();
            match field {
                "lifetime" => other.max_lifetime_secs = 0,
                "credentials" => other.api_key_env = "ACCOUNT_B".into(),
                _ => other.api_base = "https://other.invalid".into(),
            }
            let mut actions = [0; 3];
            let result = with_unambiguous([&first, &other], |_| {
                actions[0] += 1; // inventory
                actions[1] += 1; // delete
                actions[2] += 1; // ledger retirement
            });
            assert!(result.is_err(), "{field}");
            assert_eq!(actions, [0; 3]);
        }
        let mut reconciliations = 0;
        with_unambiguous([&first, &first.clone()], |_| reconciliations += 1).unwrap();
        assert_eq!(reconciliations, 1, "identical policy reconciles once");
    }
}
