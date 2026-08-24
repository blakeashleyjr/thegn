//! Multi-account fan-out with failure isolation.

use thegn_core::seam::{BoxFuture, SeamError};

/// One configured account of a seam.
pub struct Account<T: ?Sized> {
    /// The user's account name (`[[issue_accounts]] name`), for logs and
    /// per-account cache keys.
    pub name: String,
    /// The provider id (`"linear"`, `"github"`) — the prefix of routed ids.
    pub provider: &'static str,
    pub backend: Box<T>,
}

/// Accounts of one seam, in config order. `fan_out` merges every account's
/// successes and isolates a single account's failure (logged, contributes
/// nothing); `route` picks the account owning a `"<provider>:<key>"` id.
pub struct Router<T: ?Sized> {
    pub accounts: Vec<Account<T>>,
}

/// One account's outcome from [`Router::fan_out_each`].
pub struct AccountResult<R, E> {
    pub account: String,
    pub provider: &'static str,
    pub result: Result<R, E>,
}

impl<T: ?Sized> Router<T> {
    pub fn new(accounts: Vec<Account<T>>) -> Self {
        Router { accounts }
    }

    pub fn is_configured(&self) -> bool {
        !self.accounts.is_empty()
    }

    pub fn provider_ids(&self) -> Vec<&'static str> {
        self.accounts.iter().map(|a| a.provider).collect()
    }

    /// The account owning `id` (`"<provider>:<key>"`, or a bare provider id).
    /// When several accounts share a provider the first wins — a bare id
    /// cannot disambiguate accounts (a known multi-account limitation).
    pub fn route(&self, id: &str) -> Option<&Account<T>> {
        let prefix = id.split_once(':').map(|(p, _)| p).unwrap_or(id);
        self.accounts.iter().find(|a| a.provider == prefix)
    }

    /// Run `op` on every account, keeping each outcome separately so a cache
    /// can store/diff per account.
    pub async fn fan_out_each<'a, R, E, F>(&'a self, op: F) -> Vec<AccountResult<R, E>>
    where
        F: Fn(&'a T) -> BoxFuture<'a, Result<R, E>>,
    {
        let mut out = Vec::with_capacity(self.accounts.len());
        for a in &self.accounts {
            out.push(AccountResult {
                account: a.name.clone(),
                provider: a.provider,
                result: op(&a.backend).await,
            });
        }
        out
    }

    /// Run `op` on every account and concatenate the successes. A failing
    /// account is logged and contributes nothing; the call itself never
    /// fails, and no accounts ⇒ empty (not an error).
    pub async fn fan_out<'a, R, E, F>(&'a self, op_name: &'static str, op: F) -> Vec<R>
    where
        E: SeamError,
        F: Fn(&'a T) -> BoxFuture<'a, Result<Vec<R>, E>>,
    {
        let mut all = Vec::new();
        for r in self.fan_out_each(op).await {
            match r.result {
                Ok(mut items) => all.append(&mut items),
                Err(e) => tracing::warn!(
                    account = %r.account,
                    provider = r.provider,
                    op = op_name,
                    class = ?e.class(),
                    error = %e,
                    "account failed; contributing nothing"
                ),
            }
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::seam::ErrorClass;

    #[derive(Debug, PartialEq)]
    struct E;
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("boom")
        }
    }
    impl std::error::Error for E {}
    impl SeamError for E {
        fn class(&self) -> ErrorClass {
            ErrorClass::Transient
        }
        fn unsupported(_: &'static str) -> Self {
            E
        }
    }

    trait B: Send + Sync {
        fn items(&self) -> BoxFuture<'_, Result<Vec<u32>, E>>;
    }
    struct Good(Vec<u32>);
    impl B for Good {
        fn items(&self) -> BoxFuture<'_, Result<Vec<u32>, E>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }
    struct Bad;
    impl B for Bad {
        fn items(&self) -> BoxFuture<'_, Result<Vec<u32>, E>> {
            Box::pin(async move { Err(E) })
        }
    }

    fn router() -> Router<dyn B> {
        Router::new(vec![
            Account {
                name: "work".into(),
                provider: "linear",
                backend: Box::new(Good(vec![1, 2])),
            },
            Account {
                name: "oss".into(),
                provider: "github",
                backend: Box::new(Bad),
            },
            Account {
                name: "home".into(),
                provider: "jira",
                backend: Box::new(Good(vec![3])),
            },
        ])
    }

    #[tokio::test]
    async fn one_failing_account_does_not_poison_the_fan_out() {
        let r = router();
        assert_eq!(r.fan_out("items", |b| b.items()).await, vec![1, 2, 3]);
        let each = r.fan_out_each(|b| b.items()).await;
        assert_eq!(each.len(), 3);
        assert_eq!(each[1].account, "oss");
        assert_eq!(each[1].provider, "github");
        assert!(each[1].result.is_err());
    }

    #[tokio::test]
    async fn empty_router_is_empty_not_an_error() {
        let r: Router<dyn B> = Router::new(vec![]);
        assert!(!r.is_configured());
        assert!(r.fan_out("items", |b| b.items()).await.is_empty());
    }

    #[test]
    fn routes_by_id_prefix() {
        let r = router();
        assert!(r.is_configured());
        assert_eq!(r.provider_ids(), ["linear", "github", "jira"]);
        assert_eq!(r.route("jira:ABC-1").unwrap().name, "home");
        assert_eq!(r.route("linear").unwrap().name, "work");
        assert!(r.route("kaneo:1").is_none());
    }
}
