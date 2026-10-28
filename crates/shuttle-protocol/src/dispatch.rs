//! Model dispatch layer — turn a routing [`ModelTier`] into the concrete
//! provider/model environment a cell-run launch should carry (efficiency bet
//! #3, P2.2).
//!
//! [`routing::route`](crate::routing::route) reads a [`ToolOutcome`] and returns
//! *which tier* to use next. This module is the second half: given that tier,
//! resolve the exact `SHUTTLE_DEFAULT_PROVIDER` / `SHUTTLE_DEFAULT_MODEL` pair to inject
//! so the launched cell talks to the right model. It sits **above external** — external owns
//! the permission cage; dispatch only picks the model, never widens caps.
//!
//! # Why a separate, pure layer
//! The launch site (shuttle-cli `cell-run`) already knows how to read
//! `SHUTTLE_DEFAULT_PROVIDER`/`SHUTTLE_DEFAULT_MODEL` (see `config.rs`). Rather than teach
//! it about tiers, dispatch produces the *already-resolved* env pairs; the
//! launcher just applies them. Keeping it here (base crate, pure, no I/O beyond
//! reading its own env) means it is unit-testable and reusable by any launcher.
//!
//! # Tier → provider/model resolution
//! Two tiers map to two env-configured endpoints, with a single-endpoint
//! fallback so an unconfigured deployment still works:
//!
//! | tier      | provider env                         | model env                       |
//! |-----------|--------------------------------------|---------------------------------|
//! | Cheap     | `SHUTTLE_CHEAP_PROVIDER`  → `SHUTTLE_DEFAULT_PROVIDER` | `SHUTTLE_CHEAP_MODEL`  → `SHUTTLE_DEFAULT_MODEL` |
//! | Expensive | `SHUTTLE_EXPENSIVE_PROVIDER` → `SHUTTLE_DEFAULT_PROVIDER` | `SHUTTLE_EXPENSIVE_MODEL` → `SHUTTLE_DEFAULT_MODEL` |
//! | None      | (no model needed — inject nothing)   | —                               |

use crate::routing::ModelTier;

/// Env var the launcher reads for the provider (mirrors `config.rs`).
pub const PROVIDER_ENV: &str = "SHUTTLE_DEFAULT_PROVIDER";
/// Env var the launcher reads for the model (mirrors `config.rs`).
pub const MODEL_ENV: &str = "SHUTTLE_DEFAULT_MODEL";

const CHEAP_PROVIDER_ENV: &str = "SHUTTLE_CHEAP_PROVIDER";
const CHEAP_MODEL_ENV: &str = "SHUTTLE_CHEAP_MODEL";
const EXPENSIVE_PROVIDER_ENV: &str = "SHUTTLE_EXPENSIVE_PROVIDER";
const EXPENSIVE_MODEL_ENV: &str = "SHUTTLE_EXPENSIVE_MODEL";

/// A resolved provider/model pair for one tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEndpoint {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl ModelEndpoint {
    /// The `(key, value)` env pairs to inject at cell-run launch. Only present
    /// (`Some`) fields are emitted, so an unconfigured field leaves the child's
    /// existing env untouched rather than clobbering it with an empty string.
    pub fn env_pairs(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(p) = &self.provider {
            out.push((PROVIDER_ENV, p.clone()));
        }
        if let Some(m) = &self.model {
            out.push((MODEL_ENV, m.clone()));
        }
        out
    }

    /// True when neither provider nor model resolved (nothing to inject).
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
    }
}

/// How dispatch reads its tier configuration. The default reads process env;
/// tests inject a closure so no global env mutation is needed.
pub struct Dispatcher<F: Fn(&str) -> Option<String>> {
    lookup: F,
}

impl Default for Dispatcher<fn(&str) -> Option<String>> {
    fn default() -> Self {
        Dispatcher {
            lookup: env_nonempty,
        }
    }
}

/// Process-env reader that treats blank/whitespace as absent (mirrors config.rs).
pub fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl<F: Fn(&str) -> Option<String>> Dispatcher<F> {
    /// Build a dispatcher over an arbitrary lookup (used by tests).
    pub fn with_lookup(lookup: F) -> Self {
        Dispatcher { lookup }
    }

    /// Resolve the endpoint for a tier. `Cheap`/`Expensive` read their tier env
    /// with a fallback to the single-endpoint `SHUTTLE_DEFAULT_*`; `None` resolves to
    /// an empty endpoint (no model needed — the move is mechanical or done).
    pub fn endpoint_for(&self, tier: ModelTier) -> ModelEndpoint {
        match tier {
            ModelTier::None => ModelEndpoint {
                provider: None,
                model: None,
            },
            ModelTier::Cheap => ModelEndpoint {
                provider: (self.lookup)(CHEAP_PROVIDER_ENV).or_else(|| (self.lookup)(PROVIDER_ENV)),
                model: (self.lookup)(CHEAP_MODEL_ENV).or_else(|| (self.lookup)(MODEL_ENV)),
            },
            ModelTier::Expensive => ModelEndpoint {
                provider: (self.lookup)(EXPENSIVE_PROVIDER_ENV)
                    .or_else(|| (self.lookup)(PROVIDER_ENV)),
                model: (self.lookup)(EXPENSIVE_MODEL_ENV).or_else(|| (self.lookup)(MODEL_ENV)),
            },
        }
    }
}

/// Convenience: resolve the env pairs to inject for a tier, reading process env.
///
/// The launcher calls this right before spawning the cell and applies each
/// `(key, value)` to the child environment.
pub fn env_for_tier(tier: ModelTier) -> Vec<(&'static str, String)> {
    Dispatcher::default().endpoint_for(tier).env_pairs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn dispatcher(pairs: &[(&str, &str)]) -> Dispatcher<impl Fn(&str) -> Option<String>> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Dispatcher::with_lookup(move |k| map.get(k).cloned())
    }

    #[test]
    fn none_tier_injects_nothing() {
        let ep = dispatcher(&[]).endpoint_for(ModelTier::None);
        assert!(ep.is_empty());
        assert!(ep.env_pairs().is_empty());
    }

    #[test]
    fn cheap_prefers_tier_env() {
        let d = dispatcher(&[
            ("SHUTTLE_CHEAP_PROVIDER", "openai"),
            ("SHUTTLE_CHEAP_MODEL", "gpt-4o-mini"),
            ("SHUTTLE_DEFAULT_PROVIDER", "fallback"),
        ]);
        let ep = d.endpoint_for(ModelTier::Cheap);
        assert_eq!(ep.provider.as_deref(), Some("openai"));
        assert_eq!(ep.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn cheap_falls_back_to_default_env() {
        let d = dispatcher(&[
            ("SHUTTLE_DEFAULT_PROVIDER", "anthropic"),
            ("SHUTTLE_DEFAULT_MODEL", "haiku"),
        ]);
        let ep = d.endpoint_for(ModelTier::Cheap);
        assert_eq!(ep.provider.as_deref(), Some("anthropic"));
        assert_eq!(ep.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn expensive_prefers_tier_env() {
        let d = dispatcher(&[
            ("SHUTTLE_EXPENSIVE_PROVIDER", "anthropic"),
            ("SHUTTLE_EXPENSIVE_MODEL", "opus"),
            ("SHUTTLE_DEFAULT_MODEL", "haiku"),
        ]);
        let ep = d.endpoint_for(ModelTier::Expensive);
        assert_eq!(ep.provider.as_deref(), Some("anthropic"));
        assert_eq!(ep.model.as_deref(), Some("opus"));
    }

    #[test]
    fn env_pairs_emits_only_present_fields() {
        let ep = ModelEndpoint {
            provider: Some("p".into()),
            model: None,
        };
        assert_eq!(ep.env_pairs(), vec![(PROVIDER_ENV, "p".to_string())]);
    }

    #[test]
    fn unconfigured_cheap_is_empty() {
        let ep = dispatcher(&[]).endpoint_for(ModelTier::Cheap);
        assert!(ep.is_empty());
    }
}
