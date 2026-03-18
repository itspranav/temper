//! Pareto frontier management for multi-objective optimization.
//!
//! The Pareto frontier tracks the set of non-dominated candidates.
//! A candidate dominates another if it is at least as good on all
//! objectives and strictly better on at least one.

use super::candidate::Candidate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The Pareto frontier: set of non-dominated candidates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParetoFrontier {
    /// Members indexed by candidate ID.
    pub members: BTreeMap<String, Candidate>,
}

impl ParetoFrontier {
    /// Create an empty Pareto frontier.
    pub fn new() -> Self {
        Self {
            members: BTreeMap::new(),
        }
    }

    /// Check if candidate `a` dominates candidate `b`.
    ///
    /// Domination: `a` is at least as good on all objectives AND
    /// strictly better on at least one.
    pub fn dominates(a_scores: &BTreeMap<String, f64>, b_scores: &BTreeMap<String, f64>) -> bool {
        if a_scores.is_empty() || b_scores.is_empty() {
            return false;
        }

        // Collect all objective keys from both sides.
        let all_keys: std::collections::BTreeSet<&String> =
            a_scores.keys().chain(b_scores.keys()).collect();

        let mut at_least_as_good = true;
        let mut strictly_better = false;

        for key in all_keys {
            let a_val = a_scores.get(key).copied().unwrap_or(0.0);
            let b_val = b_scores.get(key).copied().unwrap_or(0.0);

            if a_val < b_val {
                at_least_as_good = false;
                break;
            }
            if a_val > b_val {
                strictly_better = true;
            }
        }

        at_least_as_good && strictly_better
    }

    /// Try to add a candidate to the frontier.
    ///
    /// Returns `true` if the candidate was added (is non-dominated).
    /// Removes any existing members that the new candidate dominates.
    pub fn try_add(&mut self, candidate: Candidate) -> bool {
        let new_scores = &candidate.scores;

        // Check if any existing member dominates the new candidate
        for existing in self.members.values() {
            if Self::dominates(&existing.scores, new_scores) {
                return false;
            }
        }

        // Remove members that the new candidate dominates
        let dominated: Vec<String> = self
            .members
            .iter()
            .filter(|(_, existing)| Self::dominates(new_scores, &existing.scores))
            .map(|(id, _)| id.clone())
            .collect();

        for id in dominated {
            self.members.remove(&id);
        }

        self.members.insert(candidate.id.clone(), candidate);
        true
    }

    /// Get the number of members in the frontier.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Check if the frontier is empty.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Select the candidate with the worst score on a given objective.
    ///
    /// Used to identify the weakest member for targeted improvement.
    pub fn weakest_on(&self, objective: &str) -> Option<&Candidate> {
        self.members
            .values()
            .filter(|c| c.scores.contains_key(objective))
            .min_by(|a, b| {
                let a_score = a.scores[objective];
                let b_score = b.scores[objective];
                a_score
                    .partial_cmp(&b_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Get all members as a sorted vec (by ID for determinism).
    pub fn members_sorted(&self) -> Vec<&Candidate> {
        self.members.values().collect()
    }
}

impl Default for ParetoFrontier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_candidate(id: &str, scores: &[(&str, f64)]) -> Candidate {
        let mut c = Candidate::new(
            id.into(),
            "spec".into(),
            "pm".into(),
            "Issue".into(),
            1,
            Utc::now(),
        );
        for (obj, score) in scores {
            c.set_score((*obj).into(), *score);
        }
        c
    }

    #[test]
    fn test_dominance_basic() {
        let a = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.8)]);
        let b = BTreeMap::from([("x".into(), 0.7), ("y".into(), 0.6)]);

        assert!(ParetoFrontier::dominates(&a, &b));
        assert!(!ParetoFrontier::dominates(&b, &a));
    }

    #[test]
    fn test_dominance_equal() {
        let a = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.8)]);
        let b = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.8)]);

        // Equal scores: neither dominates
        assert!(!ParetoFrontier::dominates(&a, &b));
        assert!(!ParetoFrontier::dominates(&b, &a));
    }

    #[test]
    fn test_dominance_tradeoff() {
        let a = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.5)]);
        let b = BTreeMap::from([("x".into(), 0.5), ("y".into(), 0.9)]);

        // Trade-off: neither dominates
        assert!(!ParetoFrontier::dominates(&a, &b));
        assert!(!ParetoFrontier::dominates(&b, &a));
    }

    #[test]
    fn test_dominance_empty_scores() {
        let empty = BTreeMap::new();
        let non_empty = BTreeMap::from([("x".into(), 0.9)]);

        assert!(!ParetoFrontier::dominates(&empty, &non_empty));
        assert!(!ParetoFrontier::dominates(&non_empty, &empty));
    }

    #[test]
    fn test_frontier_add_non_dominated() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.9), ("y", 0.5)]);
        let c2 = make_candidate("c2", &[("x", 0.5), ("y", 0.9)]);

        assert!(frontier.try_add(c1));
        assert!(frontier.try_add(c2));
        assert_eq!(frontier.len(), 2);
    }

    #[test]
    fn test_frontier_dominated_rejected() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.9), ("y", 0.8)]);
        let c2 = make_candidate("c2", &[("x", 0.7), ("y", 0.6)]);

        assert!(frontier.try_add(c1));
        assert!(!frontier.try_add(c2)); // c2 dominated by c1
        assert_eq!(frontier.len(), 1);
    }

    #[test]
    fn test_frontier_new_dominates_existing() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.7), ("y", 0.6)]);
        let c2 = make_candidate("c2", &[("x", 0.9), ("y", 0.8)]);

        assert!(frontier.try_add(c1));
        assert!(frontier.try_add(c2)); // c2 dominates c1, c1 removed
        assert_eq!(frontier.len(), 1);
        assert!(frontier.members.contains_key("c2"));
    }

    #[test]
    fn test_frontier_weakest_on() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.9), ("y", 0.3)]);
        let c2 = make_candidate("c2", &[("x", 0.3), ("y", 0.9)]);

        frontier.try_add(c1);
        frontier.try_add(c2);

        let weakest_x = frontier.weakest_on("x").unwrap();
        assert_eq!(weakest_x.id, "c2");

        let weakest_y = frontier.weakest_on("y").unwrap();
        assert_eq!(weakest_y.id, "c1");
    }

    #[test]
    fn test_frontier_serialization() {
        let mut frontier = ParetoFrontier::new();
        frontier.try_add(make_candidate("c1", &[("x", 0.8)]));

        let json = serde_json::to_string(&frontier).unwrap();
        let parsed: ParetoFrontier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
    }
}
