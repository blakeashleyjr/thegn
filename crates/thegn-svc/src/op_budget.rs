//! A total operation budget for provider wait loops (THE-529): every in-flight call and every sleep is bounded by what is left.

use std::future::Future;
use std::time::Duration;

pub(crate) struct OpBudget {
    start: tokio::time::Instant,
    total: Duration,
}

impl OpBudget {
    pub(crate) fn start(total: Duration) -> Self {
        Self {
            start: tokio::time::Instant::now(),
            total,
        }
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.start.elapsed())
    }

    pub(crate) fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    pub(crate) async fn within<F: Future>(&self, fut: F) -> Option<F::Output> {
        tokio::time::timeout(self.remaining(), fut).await.ok()
    }

    pub(crate) async fn pause(&self, poll: Duration) {
        tokio::time::sleep(poll.min(self.remaining())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::OpBudget;
    use std::future;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn within_returns_none_exactly_at_the_budget() {
        let budget = OpBudget::start(Duration::from_secs(5));
        let start = tokio::time::Instant::now();

        assert_eq!(budget.within(future::pending::<()>()).await, None);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn within_returns_the_value_before_the_budget() {
        let budget = OpBudget::start(Duration::from_secs(5));

        let value = budget
            .within(async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                7
            })
            .await;

        assert_eq!(value, Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn pause_never_overshoots() {
        let budget = OpBudget::start(Duration::from_secs(2));
        let before = tokio::time::Instant::now();

        budget.pause(Duration::from_secs(4)).await;

        assert_eq!(before.elapsed(), Duration::from_secs(2));
        assert!(budget.expired());
        assert_eq!(budget.remaining(), Duration::ZERO);
    }
}
