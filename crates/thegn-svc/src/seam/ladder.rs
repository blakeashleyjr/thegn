//! Ordered degradation across provider layers.

use thegn_core::seam::{BoxFuture, SeamError};

/// Layers tried in order for every operation. The first layer whose result is
/// `Ok` — or an error that does **not** fall through (`Auth`, `NotFound`,
/// `Transient`, …; see [`ErrorClass::falls_through`](thegn_core::seam::ErrorClass::falls_through))
/// — is the answer. A layer that can't (unsupported op, missing binary,
/// nothing configured) is skipped. If every layer falls through, the **last**
/// layer's error is returned: by construction that is the most basic layer
/// (the CLI fallback), whose "not installed" message is the actionable one.
pub struct Ladder<T: ?Sized> {
    pub id: &'static str,
    pub layers: Vec<Box<T>>,
}

impl<T: ?Sized> Ladder<T> {
    pub fn new(id: &'static str, layers: Vec<Box<T>>) -> Self {
        Ladder { id, layers }
    }

    /// Run `op` down the ladder (async).
    pub async fn try_each<'a, R, E, F>(&'a self, op_name: &'static str, op: F) -> Result<R, E>
    where
        E: SeamError,
        F: Fn(&'a T) -> BoxFuture<'a, Result<R, E>>,
    {
        let mut last: Option<E> = None;
        for layer in &self.layers {
            match op(layer).await {
                Ok(r) => return Ok(r),
                Err(e) if e.falls_through() => {
                    tracing::debug!(
                        ladder = self.id,
                        op = op_name,
                        class = ?e.class(),
                        "layer fell through"
                    );
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| E::unsupported(op_name)))
    }

    /// Run `op` down the ladder (sync seams).
    pub fn try_each_sync<R, E, F>(&self, op_name: &'static str, op: F) -> Result<R, E>
    where
        E: SeamError,
        F: Fn(&T) -> Result<R, E>,
    {
        let mut last: Option<E> = None;
        for layer in &self.layers {
            match op(layer) {
                Ok(r) => return Ok(r),
                Err(e) if e.falls_through() => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| E::unsupported(op_name)))
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::seam::ErrorClass;

    #[derive(Debug, PartialEq)]
    struct E(ErrorClass, &'static str);
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}:{}", self.0, self.1)
        }
    }
    impl std::error::Error for E {}
    impl SeamError for E {
        fn class(&self) -> ErrorClass {
            self.0
        }
        fn unsupported(op: &'static str) -> Self {
            E(ErrorClass::Unsupported, op)
        }
    }

    trait Layer: Send + Sync {
        fn name(&self) -> &'static str;
        fn answer(&self) -> Result<&'static str, E>;
        fn answer_async(&self) -> BoxFuture<'_, Result<&'static str, E>> {
            Box::pin(async move { self.answer() })
        }
    }
    struct L(&'static str, Result<&'static str, E>);
    impl Layer for L {
        fn name(&self) -> &'static str {
            self.0
        }
        fn answer(&self) -> Result<&'static str, E> {
            match &self.1 {
                Ok(v) => Ok(v),
                Err(e) => Err(E(e.0, e.1)),
            }
        }
    }

    fn ladder(layers: Vec<L>) -> Ladder<dyn Layer> {
        Ladder::new(
            "t",
            layers
                .into_iter()
                .map(|l| Box::new(l) as Box<dyn Layer>)
                .collect(),
        )
    }

    #[tokio::test]
    async fn falls_through_unsupported_and_not_installed() {
        let l = ladder(vec![
            L("native", Err(E(ErrorClass::Unsupported, "x"))),
            L("cli", Err(E(ErrorClass::NotInstalled, "gh"))),
            L("last", Ok("hit")),
        ]);
        assert_eq!(l.try_each("x", |x| x.answer_async()).await, Ok("hit"));
        assert_eq!(l.try_each_sync("x", |x| x.answer()), Ok("hit"));
    }

    #[tokio::test]
    async fn stops_on_a_final_error() {
        let l = ladder(vec![
            L("native", Err(E(ErrorClass::Auth, "bad token"))),
            L("cli", Ok("never")),
        ]);
        assert_eq!(
            l.try_each("x", |x| x.answer_async()).await,
            Err(E(ErrorClass::Auth, "bad token"))
        );
        assert_eq!(
            l.try_each_sync("x", |x| x.answer()),
            Err(E(ErrorClass::Auth, "bad token"))
        );
    }

    #[tokio::test]
    async fn all_fall_through_returns_last_layers_error() {
        let l = ladder(vec![
            L("native", Err(E(ErrorClass::NotConfigured, "no token"))),
            L("cli", Err(E(ErrorClass::NotInstalled, "gh"))),
        ]);
        assert_eq!(
            l.try_each("x", |x| x.answer_async()).await,
            Err(E(ErrorClass::NotInstalled, "gh"))
        );
    }

    #[tokio::test]
    async fn empty_ladder_is_unsupported() {
        let l = ladder(vec![]);
        assert!(l.is_empty());
        assert_eq!(
            l.try_each("op", |x| x.answer_async()).await,
            Err(E(ErrorClass::Unsupported, "op"))
        );
        assert_eq!(
            l.try_each_sync("op", |x| x.answer()),
            Err(E(ErrorClass::Unsupported, "op"))
        );
        // `name` is part of the trait so `dyn Layer` is exercised as a layer.
        assert_eq!(L("n", Ok("")).name(), "n");
    }
}
