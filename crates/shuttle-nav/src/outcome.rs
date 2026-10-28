//! Lowering of `shuttle-nav`'s native query results into the universal
//! [`shuttle_protocol::ToolOutcome`] envelope (efficiency bet #2, P1.2).
//!
//! `shuttle-nav` sits above `shuttle-protocol`, so the mapping lives here, not in the base
//! crate. Each navigation query (`defs`/`references`/`callers`/`impls`/`hover`)
//! returns `Result<Vec<NavHit>, NavError>`; this module folds that into a single
//! [`ToolOutcome`] a router can read without parsing prose.
//!
//! # The three-state mapping (§7 never-guess)
//! * **Hits found, all `High`** → [`Green`] (`ok`). The caller can trust the
//!   result directly.
//! * **Hits found, but some `Low`/`Medium`** → [`Green`] but `ok-low-confidence`
//!   (`green_low`), and the low-confidence sites are surfaced in `whats_missing`
//!   so a router may choose to escalate for verification — *without* treating a
//!   real hit as a failure.
//! * **Zero hits** → [`Blocked`] (`missing-definition`). Not an error: the one
//!   concrete next action is "widen the scan scope / provide the definition".
//!   Nav never fabricates a hit to look Green.
//! * **`NavError`** → [`Error`]. IO is `io`; the symbol/locate backends are
//!   `internal` (they broke, not the user's input).
//!
//! [`Green`]: shuttle_protocol::OutcomeState::Green
//! [`Blocked`]: shuttle_protocol::OutcomeState::Blocked
//! [`Error`]: shuttle_protocol::OutcomeState::Error

use crate::{Confidence, NavError, NavHit};
use shuttle_protocol::{
    reason, NavDetail, NavHitLite, OutcomeConfidence, ToolOutcome, TypedDetail,
};

/// Tool name used in every nav-produced envelope.
pub const TOOL_NAV: &str = "nav";

/// Map `shuttle-nav`'s canonical [`Confidence`] onto the protocol mirror. 1:1.
pub fn lower_confidence(c: Confidence) -> OutcomeConfidence {
    match c {
        Confidence::High => OutcomeConfidence::High,
        Confidence::Medium => OutcomeConfidence::Medium,
        Confidence::Low => OutcomeConfidence::Low,
    }
}

fn lite(h: &NavHit) -> NavHitLite {
    NavHitLite {
        symbol: h.symbol.clone(),
        path: h.path.clone(),
        line: h.line,
        confidence: lower_confidence(h.confidence),
    }
}

/// Lower a nav query result into a [`ToolOutcome`].
///
/// * `query` is the query name (`"defs"`/`"references"`/`"callers"`/`"impls"`/
///   `"hover"`), carried into [`NavDetail::query`].
/// * `target` is the symbol/type the query was about.
pub fn nav_outcome(
    query: &str,
    target: &str,
    result: Result<Vec<NavHit>, NavError>,
) -> ToolOutcome {
    match result {
        Ok(hits) => {
            let detail = TypedDetail::Nav(NavDetail {
                query: query.to_string(),
                target: target.to_string(),
                hits: hits.iter().map(lite).collect(),
            });

            if hits.is_empty() {
                // Zero hits is one-step-fixable, not an error (§7): the next
                // action is to widen scope or supply the missing definition.
                return ToolOutcome::blocked(
                    TOOL_NAV,
                    reason::MISSING_DEFINITION,
                    format!(
                        "no `{query}` hit for `{target}`; re-run with a wider --scan-scope or provide the definition"
                    ),
                    detail,
                )
                .with_missing(vec![format!("{target} ({query}): 0 hits in scope")]);
            }

            let low: Vec<String> = hits
                .iter()
                .filter(|h| !matches!(h.confidence, Confidence::High))
                .map(|h| format!("{} @ {}:{}", h.symbol, h.path, h.line))
                .collect();

            if low.is_empty() {
                ToolOutcome::green(TOOL_NAV, detail).with_summary(format!(
                    "{query} `{target}`: {} hit(s), all high-confidence",
                    hits.len()
                ))
            } else {
                // Real hits, but some are shaky — Green-but-low so a router may
                // verify the flagged sites without discarding the result.
                ToolOutcome::green_low(TOOL_NAV, detail)
                    .with_missing(low)
                    .with_summary(format!(
                        "{query} `{target}`: {} hit(s), some low-confidence",
                        hits.len()
                    ))
            }
        }
        Err(e) => ToolOutcome::error(TOOL_NAV, nav_error_reason(&e), e.to_string()),
    }
}

/// Map a [`NavError`] onto a stable [`reason`] code. IO failures are `io`; the
/// symbol/locate backends breaking is `internal` (a bug, not user input).
fn nav_error_reason(e: &NavError) -> &'static str {
    match e {
        NavError::Io(_) => reason::IO,
        NavError::Json(_) => reason::INVALID_INPUT,
        NavError::Symbols(_) | NavError::Locate(_) => reason::INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, NavKind};
    use shuttle_protocol::OutcomeState;

    fn hit(symbol: &str, conf: Confidence) -> NavHit {
        NavHit {
            path: "m.py".into(),
            line: 3,
            col: None,
            kind: NavKind::Definition,
            symbol: symbol.into(),
            snippet: None,
            confidence: conf,
            source: Backend::Ctags,
        }
    }

    #[test]
    fn zero_hits_blocks_missing_definition() {
        let o = nav_outcome("defs", "Widget", Ok(vec![]));
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::MISSING_DEFINITION);
        assert!(!o.state.is_error());
        assert_eq!(o.whats_missing.len(), 1);
    }

    #[test]
    fn all_high_hits_are_green() {
        let o = nav_outcome("defs", "Widget", Ok(vec![hit("Widget", Confidence::High)]));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
    }

    #[test]
    fn some_low_hits_are_green_low_with_flagged_sites() {
        let o = nav_outcome(
            "references",
            "Widget",
            Ok(vec![
                hit("Widget", Confidence::High),
                hit("Widget", Confidence::Low),
            ]),
        );
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK_LOW_CONFIDENCE);
        assert_eq!(o.whats_missing.len(), 1);
    }
}
