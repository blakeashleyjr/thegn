use anyhow::{Result, anyhow};

pub(crate) trait EgressSteps {
    fn apply(&self, id: &str, allow: &[String], block: &[String]) -> Result<()>;
    fn read_back(&self, id: &str) -> Result<Vec<thegn_svc::provider::PolicyRule>>;
    fn destroy(&self, id: &str) -> Result<()>;
}

pub(crate) fn admit_egress(
    steps: &dyn EgressSteps,
    id: &str,
    allow: &[String],
    block: &[String],
) -> Result<()> {
    let verification = thegn_svc::provider::rules_from(allow, block)
        .map_err(anyhow::Error::from)
        .and_then(|expected| {
            steps.apply(id, allow, block).and_then(|()| {
                steps.read_back(id).and_then(|observed| {
                    if observed == expected {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "observed policy differs from the requested policy (expected {} rules, observed {})",
                            expected.len(),
                            observed.len()
                        ))
                    }
                })
            })
        });
    match verification {
        Ok(()) => Ok(()),
        Err(reason) => match steps.destroy(id) {
            Ok(()) => Err(anyhow!(
                "network policy for sandbox {id} could not be verified ({reason}); the sandbox was destroyed"
            )),
            Err(destroy_error) => Err(anyhow!(
                "network policy for sandbox {id} could not be verified ({reason}) and destroying it failed ({destroy_error}): it is running WITHOUT the declared egress policy; destroy it with `thegn env deprovision {id}`"
            )),
        },
    }
}

pub(crate) fn verify_existing_egress(
    steps: &dyn EgressSteps,
    id: &str,
    allow: &[String],
    block: &[String],
) -> Result<()> {
    let expected = thegn_svc::provider::rules_from(allow, block).map_err(|reason| {
        anyhow!(
            "network policy for existing sandbox {id} is invalid ({reason}); refusing to provision it"
        )
    })?;
    let initial_read = steps.read_back(id);
    if let Ok(observed) = &initial_read
        && observed == &expected
    {
        return Ok(());
    }

    let verification = steps.apply(id, allow, block).and_then(|()| {
        steps.read_back(id).and_then(|observed| {
            if observed == expected {
                Ok(())
            } else {
                Err(anyhow!(
                    "observed policy differs from the requested policy (expected {} rules, observed {})",
                    expected.len(),
                    observed.len()
                ))
            }
        })
    });
    verification.map_err(|reason| {
        anyhow!(
            "network policy for existing sandbox {id} could not be verified ({reason}); refusing to provision it"
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EgressAdmission {
    Skip,
    AdmitFresh,
    VerifyExisting,
}

pub(crate) fn egress_admission(
    created: bool,
    caps_egress: bool,
    allow: &[String],
    block: &[String],
) -> EgressAdmission {
    if !caps_egress || (allow.is_empty() && block.is_empty()) {
        EgressAdmission::Skip
    } else if created {
        EgressAdmission::AdmitFresh
    } else {
        EgressAdmission::VerifyExisting
    }
}

pub(crate) struct BlockingProviderEgress<'a> {
    pub provider: &'a thegn_svc::provider::Provider,
}

impl EgressSteps for BlockingProviderEgress<'_> {
    fn apply(&self, id: &str, allow: &[String], block: &[String]) -> Result<()> {
        crate::agent::block_on_provider(|| async {
            self.provider.set_network_policy(id, allow, block).await
        })
    }

    fn read_back(&self, id: &str) -> Result<Vec<thegn_svc::provider::PolicyRule>> {
        crate::agent::block_on_provider(|| async { self.provider.get_network_policy(id).await })
    }

    fn destroy(&self, id: &str) -> Result<()> {
        crate::agent::block_on_provider(|| async { self.provider.destroy(id).await })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EgressAdmission, EgressSteps, admit_egress, egress_admission, verify_existing_egress,
    };
    use std::cell::RefCell;
    use thegn_svc::provider::{PolicyRule, rules_from};

    struct ScriptedEgress {
        calls: RefCell<Vec<String>>,
        apply_results: RefCell<Vec<anyhow::Result<()>>>,
        read_results: RefCell<Vec<anyhow::Result<Vec<PolicyRule>>>>,
    }

    impl ScriptedEgress {
        fn new(
            apply_results: Vec<anyhow::Result<()>>,
            read_results: Vec<anyhow::Result<Vec<PolicyRule>>>,
        ) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                apply_results: RefCell::new(apply_results),
                read_results: RefCell::new(read_results),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl EgressSteps for ScriptedEgress {
        fn apply(&self, id: &str, _allow: &[String], _block: &[String]) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("apply:{id}"));
            self.apply_results.borrow_mut().remove(0)
        }

        fn read_back(&self, id: &str) -> anyhow::Result<Vec<PolicyRule>> {
            self.calls.borrow_mut().push(format!("read:{id}"));
            self.read_results.borrow_mut().remove(0)
        }

        fn destroy(&self, id: &str) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("destroy:{id}"));
            Ok(())
        }
    }

    fn lists() -> (Vec<String>, Vec<String>) {
        (vec!["allowed.test".into()], vec!["blocked.test".into()])
    }

    #[test]
    fn existing_matching_policy_is_not_reapplied() {
        let (allow, block) = lists();
        let steps = ScriptedEgress::new(vec![], vec![Ok(rules_from(&allow, &block).unwrap())]);

        assert!(verify_existing_egress(&steps, "sandbox-1", &allow, &block).is_ok());
        assert_eq!(steps.calls(), ["read:sandbox-1"]);
    }

    #[test]
    fn existing_mismatch_is_reapplied_and_verified() {
        let (allow, block) = lists();
        let steps = ScriptedEgress::new(
            vec![Ok(())],
            vec![Ok(Vec::new()), Ok(rules_from(&allow, &block).unwrap())],
        );

        assert!(verify_existing_egress(&steps, "sandbox-1", &allow, &block).is_ok());
        assert_eq!(
            steps.calls(),
            ["read:sandbox-1", "apply:sandbox-1", "read:sandbox-1"]
        );
    }

    #[test]
    fn existing_read_error_is_reapplied_and_verified() {
        let (allow, block) = lists();
        let steps = ScriptedEgress::new(
            vec![Ok(())],
            vec![
                Err(anyhow::anyhow!("read failed")),
                Ok(rules_from(&allow, &block).unwrap()),
            ],
        );

        assert!(verify_existing_egress(&steps, "sandbox-1", &allow, &block).is_ok());
        assert_eq!(
            steps.calls(),
            ["read:sandbox-1", "apply:sandbox-1", "read:sandbox-1"]
        );
    }

    #[test]
    fn existing_unverifiable_refuses_without_destroy() {
        let (allow, block) = lists();
        let failed_apply = ScriptedEgress::new(
            vec![Err(anyhow::anyhow!("apply failed"))],
            vec![Ok(Vec::new())],
        );
        let apply_result = verify_existing_egress(&failed_apply, "sandbox-1", &allow, &block);
        assert!(apply_result.is_err());
        assert!(
            apply_result
                .err()
                .is_some_and(|error| error.to_string().contains("refusing to provision"))
        );
        assert_eq!(failed_apply.calls(), ["read:sandbox-1", "apply:sandbox-1"]);

        let failed_verification =
            ScriptedEgress::new(vec![Ok(())], vec![Ok(Vec::new()), Ok(Vec::new())]);
        let read_result = verify_existing_egress(&failed_verification, "sandbox-1", &allow, &block);
        assert!(read_result.is_err());
        assert!(
            read_result
                .err()
                .is_some_and(|error| error.to_string().contains("refusing to provision"))
        );
        assert_eq!(
            failed_verification.calls(),
            ["read:sandbox-1", "apply:sandbox-1", "read:sandbox-1"]
        );
    }

    #[test]
    fn fresh_admission_with_an_invalid_entry_destroys_without_applying() {
        let allow = vec!["192.0.2.1".to_string()];
        let steps = ScriptedEgress::new(vec![], vec![]);

        let result = admit_egress(&steps, "sandbox-1", &allow, &[]);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .is_some_and(|error| { error.to_string().contains("192.0.2.1") })
        );
        assert_eq!(steps.calls(), ["destroy:sandbox-1"]);
    }

    #[test]
    fn existing_with_an_invalid_entry_refuses_without_provider_calls() {
        let allow = vec!["192.0.2.1".to_string()];
        let steps = ScriptedEgress::new(vec![], vec![]);
        let result = verify_existing_egress(&steps, "sandbox-1", &allow, &[]);

        assert!(result.is_err());
        assert!(result.err().is_some_and(|error| {
            let message = error.to_string();
            message.contains("refusing to provision") && message.contains("192.0.2.1")
        }));
        assert!(steps.calls().is_empty());
    }

    #[test]
    fn admission_decision_table() {
        let allow = vec!["allowed.test".to_string()];
        let block = vec!["blocked.test".to_string()];
        let empty: Vec<String> = Vec::new();

        assert_eq!(
            egress_admission(false, false, &empty, &empty),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(false, false, &allow, &empty),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(false, false, &empty, &block),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(true, false, &empty, &empty),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(true, false, &allow, &empty),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(true, false, &empty, &block),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(false, true, &empty, &empty),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(false, true, &allow, &empty),
            EgressAdmission::VerifyExisting
        );
        assert_eq!(
            egress_admission(false, true, &empty, &block),
            EgressAdmission::VerifyExisting
        );
        assert_eq!(
            egress_admission(true, true, &empty, &empty),
            EgressAdmission::Skip
        );
        assert_eq!(
            egress_admission(true, true, &allow, &empty),
            EgressAdmission::AdmitFresh
        );
        assert_eq!(
            egress_admission(true, true, &empty, &block),
            EgressAdmission::AdmitFresh
        );
    }

    #[test]
    fn fresh_admission_keeps_destroy_on_failure() {
        let (allow, block) = lists();
        let steps = ScriptedEgress::new(vec![Err(anyhow::anyhow!("apply failed"))], vec![]);

        assert!(admit_egress(&steps, "sandbox-1", &allow, &block).is_err());
        assert_eq!(steps.calls(), ["apply:sandbox-1", "destroy:sandbox-1"]);
    }
}
