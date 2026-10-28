//! Named design-principle proposition wire contract — `design_principle/v1`.
//!
//! A principle is a named boolean-formula template (QF_LIA-shaped:
//! conjunctions/disjunctions/negations over atomic references) whose truth
//! *reduces* to the truth of its referenced [`CoordAtom`]s via Kleene
//! three-valued propagation. A principle never invents truth: it can only
//! combine the truths its atoms already carry, and if any load-bearing atom is
//! `null`, the reduction stays `null` (never guessed to `true`).
//!
//! `is_geometric_proxy` marks a principle whose formula is only a
//! sufficient-condition *proxy* for a higher intent (e.g. "visually grouped"
//! approximating "semantically grouped"). A proxy `true` may be trusted; a
//! proxy `false` is a blind-spot and must be routed to vetting — the reduction
//! itself never fabricates the higher intent.

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, ClosedContractError};
use crate::symbol_footprint::{invalid, validate_text_id};
use crate::ui_atom::Holds;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DESIGN_PRINCIPLE_SCHEMA: &str = "design_principle/v1";

/// A boolean formula over atom references, evaluated under Kleene logic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Formula {
    /// A leaf: the truth of the referenced atom (by its `atom_id`).
    Atom { atom_ref: Sha256Digest },
    /// Conjunction. An empty `all` is vacuously `true`.
    All { all: Vec<Formula> },
    /// Disjunction. An empty `any` is vacuously `false`.
    Any { any: Vec<Formula> },
    /// Negation.
    Not { not: Box<Formula> },
}

impl Formula {
    /// Collect every referenced atom id (used to bind and to validate coverage).
    pub fn atom_refs(&self, out: &mut Vec<Sha256Digest>) {
        match self {
            Formula::Atom { atom_ref } => out.push(atom_ref.clone()),
            Formula::All { all } => all.iter().for_each(|f| f.atom_refs(out)),
            Formula::Any { any } => any.iter().for_each(|f| f.atom_refs(out)),
            Formula::Not { not } => not.atom_refs(out),
        }
    }

    /// Reduce to a Kleene truth given a resolver from atom id to its `holds`.
    /// A missing atom resolves to `null` — an unbound reference is an unknown,
    /// never a guessed truth.
    pub fn reduce(&self, holds_of: &dyn Fn(&Sha256Digest) -> Option<Holds>) -> Holds {
        match self {
            Formula::Atom { atom_ref } => holds_of(atom_ref).unwrap_or(Holds::Null),
            Formula::All { all } => all
                .iter()
                .map(|f| f.reduce(holds_of))
                .fold(Holds::True, Holds::and),
            Formula::Any { any } => any
                .iter()
                .map(|f| f.reduce(holds_of))
                .fold(Holds::False, Holds::or),
            Formula::Not { not } => not.reduce(holds_of).negate(),
        }
    }

    fn depth(&self) -> usize {
        match self {
            Formula::Atom { .. } => 1,
            Formula::All { all } => 1 + all.iter().map(Formula::depth).max().unwrap_or(0),
            Formula::Any { any } => 1 + any.iter().map(Formula::depth).max().unwrap_or(0),
            Formula::Not { not } => 1 + not.depth(),
        }
    }

    fn validate(&self) -> Result<(), ClosedContractError> {
        // A formula must reference at least one atom somewhere; empty leaf-free
        // trees carry no evidence.
        let mut refs = Vec::new();
        self.atom_refs(&mut refs);
        if refs.is_empty() {
            return Err(invalid("formula must reference at least one atom"));
        }
        if self.depth() > 32 {
            return Err(invalid("formula nesting exceeds the depth-32 limit"));
        }
        Ok(())
    }
}

/// A named design principle whose truth reduces to its atoms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesignPrinciple {
    pub schema_version: String,
    pub principle_id: Sha256Digest,
    /// The principle's stable name, e.g. `FormAligned`, `Grouped`, `Hierarchy`,
    /// `Enclosed`.
    pub name: String,
    pub formula: Formula,
    /// Whether this principle is only a sufficient-condition geometric proxy for
    /// a higher (semantic/intent) property.
    pub is_geometric_proxy: bool,
}

#[derive(Serialize)]
struct DesignPrincipleIdentity<'a> {
    name: &'a str,
    formula: &'a Formula,
    is_geometric_proxy: bool,
}

/// The outcome of reducing a principle against a set of atom truths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrincipleVerdict {
    pub holds: Holds,
    /// Whether this verdict must be routed to human/model vetting rather than
    /// mechanically trusted. A geometric-proxy `false` is a blind-spot and is
    /// always routed; a decided non-proxy verdict is mechanical.
    pub needs_vetting: bool,
}

impl DesignPrinciple {
    pub fn new(
        name: impl Into<String>,
        formula: Formula,
        is_geometric_proxy: bool,
    ) -> Result<Self, ClosedContractError> {
        let mut principle = Self {
            schema_version: DESIGN_PRINCIPLE_SCHEMA.to_string(),
            principle_id: placeholder_digest(),
            name: name.into(),
            formula,
            is_geometric_proxy,
        };
        principle.validate_fields()?;
        principle.principle_id = principle.expected_principle_id()?;
        Ok(principle)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        let principle: Self = serde_json::from_slice(bytes)?;
        principle.validate()?;
        Ok(principle)
    }

    pub fn expected_principle_id(&self) -> Result<Sha256Digest, ClosedContractError> {
        canonical_digest(&self.identity())
    }

    fn identity(&self) -> DesignPrincipleIdentity<'_> {
        DesignPrincipleIdentity {
            name: &self.name,
            formula: &self.formula,
            is_geometric_proxy: self.is_geometric_proxy,
        }
    }

    /// Reduce the principle against a map of `atom_id -> holds`.
    pub fn reduce(&self, atom_truths: &BTreeMap<Sha256Digest, Holds>) -> PrincipleVerdict {
        let holds = self.formula.reduce(&|id| atom_truths.get(id).copied());
        // A geometric-proxy false never mechanically refutes the higher intent:
        // route it to vetting. An unknown is likewise not mechanically trusted.
        let needs_vetting = match holds {
            Holds::False => self.is_geometric_proxy,
            Holds::Null => true,
            Holds::True => false,
        };
        PrincipleVerdict {
            holds,
            needs_vetting,
        }
    }

    /// Every atom this principle depends on.
    pub fn atom_refs(&self) -> Vec<Sha256Digest> {
        let mut out = Vec::new();
        self.formula.atom_refs(&mut out);
        out
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != DESIGN_PRINCIPLE_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        self.validate_fields()?;
        if self.principle_id != self.expected_principle_id()? {
            return Err(invalid(
                "principle_id does not match the canonical principle",
            ));
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        validate_text_id("name", &self.name)?;
        self.formula.validate()
    }
}

fn placeholder_digest() -> Sha256Digest {
    Sha256Digest::parse(format!("sha256:{}", "0".repeat(64)))
        .expect("fixed placeholder digest is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom_id(byte: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
    }

    fn grouped() -> DesignPrinciple {
        // Grouped := adjacency(a,b) AND aligned(a,b)
        let formula = Formula::All {
            all: vec![
                Formula::Atom {
                    atom_ref: atom_id('a'),
                },
                Formula::Atom {
                    atom_ref: atom_id('b'),
                },
            ],
        };
        DesignPrinciple::new("Grouped", formula, true).unwrap()
    }

    #[test]
    fn reduces_via_kleene() {
        let p = grouped();
        let mut truths = BTreeMap::new();
        truths.insert(atom_id('a'), Holds::True);
        truths.insert(atom_id('b'), Holds::True);
        assert_eq!(p.reduce(&truths).holds, Holds::True);

        truths.insert(atom_id('b'), Holds::Null);
        let v = p.reduce(&truths);
        assert_eq!(v.holds, Holds::Null);
        assert!(v.needs_vetting);

        truths.insert(atom_id('b'), Holds::False);
        let v = p.reduce(&truths);
        assert_eq!(v.holds, Holds::False);
        // Grouped is a geometric proxy, so a false routes to vetting.
        assert!(v.needs_vetting);
    }

    #[test]
    fn missing_atom_is_null_not_guessed() {
        let p = grouped();
        let truths = BTreeMap::new();
        assert_eq!(p.reduce(&truths).holds, Holds::Null);
    }

    #[test]
    fn non_proxy_false_is_mechanical() {
        let formula = Formula::Atom {
            atom_ref: atom_id('a'),
        };
        let p = DesignPrinciple::new("HardEnclosed", formula, false).unwrap();
        let mut truths = BTreeMap::new();
        truths.insert(atom_id('a'), Holds::False);
        let v = p.reduce(&truths);
        assert_eq!(v.holds, Holds::False);
        assert!(!v.needs_vetting);
    }

    #[test]
    fn round_trip_and_reject_unknown_fields() {
        let p = grouped();
        let bytes = serde_json::to_vec(&p).unwrap();
        assert_eq!(DesignPrinciple::parse(&bytes).unwrap(), p);

        let mut value = serde_json::to_value(&p).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(true));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(DesignPrinciple::parse(&bytes).is_err());
    }

    #[test]
    fn stable_identity() {
        let a = grouped();
        let b = grouped();
        assert_eq!(a.principle_id, b.principle_id);
    }

    #[test]
    fn empty_formula_rejected() {
        let formula = Formula::All { all: vec![] };
        assert!(DesignPrinciple::new("Empty", formula, false).is_err());
    }

    #[test]
    fn nested_or_and_not() {
        // Hierarchy := NOT(any-empty) is not meaningful; build a real tree.
        let formula = Formula::Any {
            any: vec![
                Formula::Atom {
                    atom_ref: atom_id('a'),
                },
                Formula::Not {
                    not: Box::new(Formula::Atom {
                        atom_ref: atom_id('b'),
                    }),
                },
            ],
        };
        let p = DesignPrinciple::new("Hierarchy", formula, true).unwrap();
        let mut truths = BTreeMap::new();
        truths.insert(atom_id('a'), Holds::False);
        truths.insert(atom_id('b'), Holds::False);
        // false OR NOT(false) = false OR true = true
        assert_eq!(p.reduce(&truths).holds, Holds::True);
    }
}
