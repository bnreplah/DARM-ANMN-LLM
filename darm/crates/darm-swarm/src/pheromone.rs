//! Pheromone trails over concrete routes, used for ant-colony routing.
//!
//! A route is `(peer node, model)`. Successful requests deposit `Trail`
//! pheromone scaled by how fast they were; failures deposit `Danger`.
//! Intensities decay exponentially with a half-life, so the field forgets
//! stale routes on its own. Ranking follows the ACO rule
//! `p ∝ trail^α · heuristic^β / (1 + danger)`, with a small exploration
//! share so new routes (scouts) still get traffic.

use crate::rng::SplitMix64;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteKey {
    /// Mesh peer id serving the model, empty for "host decides".
    pub node: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PheromoneKind {
    Trail,
    Danger,
}

#[derive(Debug, Clone, Copy, Default)]
struct Deposit {
    intensity: f64,
    updated_ms: u64,
}

impl Deposit {
    fn current(&self, now_ms: u64, half_life_ms: f64) -> f64 {
        let age = now_ms.saturating_sub(self.updated_ms) as f64;
        self.intensity * 0.5f64.powf(age / half_life_ms)
    }
}

#[derive(Debug, Clone)]
pub struct PheromoneField {
    half_life_ms: f64,
    alpha: f64,
    beta: f64,
    exploration: f64,
    max_routes: usize,
    deposits: HashMap<(RouteKey, PheromoneKind), Deposit>,
}

/// Upper bound on a single deposit so one route cannot saturate the field.
const MAX_INTENSITY: f64 = 50.0;

impl Default for PheromoneField {
    fn default() -> Self {
        Self::new(10 * 60 * 1_000)
    }
}

impl PheromoneField {
    pub fn new(half_life_ms: u64) -> Self {
        Self {
            half_life_ms: half_life_ms.max(1) as f64,
            alpha: 1.0,
            beta: 1.0,
            exploration: 0.1,
            max_routes: 4_096,
            deposits: HashMap::new(),
        }
    }

    pub fn with_exploration(mut self, exploration: f64) -> Self {
        self.exploration = exploration.clamp(0.0, 1.0);
        self
    }

    pub fn deposit(&mut self, route: RouteKey, kind: PheromoneKind, amount: f64, now_ms: u64) {
        if !amount.is_finite() || amount <= 0.0 {
            return;
        }
        if self.deposits.len() >= self.max_routes {
            self.evaporate(now_ms);
        }
        let half_life = self.half_life_ms;
        let deposit = self.deposits.entry((route, kind)).or_default();
        deposit.intensity = (deposit.current(now_ms, half_life) + amount).min(MAX_INTENSITY);
        deposit.updated_ms = now_ms;
    }

    /// Record a completed request on `route`.
    pub fn record_outcome(
        &mut self,
        route: &RouteKey,
        success: bool,
        latency_ms: f64,
        now_ms: u64,
    ) {
        if success {
            // Faster answers lay stronger trails (1.0 at <=1s, fading after).
            let speed = (1_000.0 / latency_ms.max(1.0)).clamp(0.05, 1.0);
            self.deposit(route.clone(), PheromoneKind::Trail, 0.5 + speed, now_ms);
        } else {
            self.deposit(route.clone(), PheromoneKind::Danger, 1.0, now_ms);
        }
    }

    pub fn intensity(&self, route: &RouteKey, kind: PheromoneKind, now_ms: u64) -> f64 {
        self.deposits
            .get(&(route.clone(), kind))
            .map_or(0.0, |d| d.current(now_ms, self.half_life_ms))
    }

    /// Drop trails that have decayed to nothing.
    pub fn evaporate(&mut self, now_ms: u64) {
        let half_life = self.half_life_ms;
        self.deposits
            .retain(|_, d| d.current(now_ms, half_life) > 0.01);
    }

    /// Desirability of a route given a heuristic (e.g. model quality or
    /// reputation, > 0).
    pub fn desirability(&self, route: &RouteKey, heuristic: f64, now_ms: u64) -> f64 {
        let trail = 1.0 + self.intensity(route, PheromoneKind::Trail, now_ms);
        let danger = self.intensity(route, PheromoneKind::Danger, now_ms);
        trail.powf(self.alpha) * heuristic.max(1e-6).powf(self.beta) / (1.0 + danger)
    }

    /// Order routes for fallback: sample the first choice by desirability
    /// (exploration share uniform), then the rest best-first.
    pub fn rank(&self, routes: &[(RouteKey, f64)], now_ms: u64, seed: u64) -> Vec<RouteKey> {
        if routes.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(RouteKey, f64)> = routes
            .iter()
            .map(|(route, heuristic)| (route.clone(), self.desirability(route, *heuristic, now_ms)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut rng = SplitMix64::new(seed);
        let first = if rng.next_f64() < self.exploration {
            rng.below(scored.len())
        } else {
            let total: f64 = scored.iter().map(|(_, w)| w).sum();
            let mut pick = rng.next_f64() * total;
            let mut chosen = 0;
            for (i, (_, weight)) in scored.iter().enumerate() {
                if pick < *weight {
                    chosen = i;
                    break;
                }
                pick -= weight;
                chosen = i;
            }
            chosen
        };
        let lead = scored.remove(first);
        std::iter::once(lead.0)
            .chain(scored.into_iter().map(|(route, _)| route))
            .collect()
    }

    pub fn snapshot(&self, now_ms: u64) -> Vec<(RouteKey, PheromoneKind, f64)> {
        let mut out: Vec<_> = self
            .deposits
            .iter()
            .map(|((route, kind), d)| (route.clone(), *kind, d.current(now_ms, self.half_life_ms)))
            .filter(|(_, _, v)| *v > 0.01)
            .collect();
        out.sort_by(|a, b| b.2.total_cmp(&a.2));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(n: &str) -> RouteKey {
        RouteKey {
            node: n.into(),
            model: "m".into(),
        }
    }

    #[test]
    fn trails_decay_by_half_life() {
        let mut field = PheromoneField::new(1_000);
        field.deposit(route("a"), PheromoneKind::Trail, 8.0, 0);
        let later = field.intensity(&route("a"), PheromoneKind::Trail, 1_000);
        assert!((later - 4.0).abs() < 1e-9);
        field.evaporate(100_000);
        assert!(field.snapshot(100_000).is_empty());
    }

    #[test]
    fn successful_routes_win_and_failing_routes_lose() {
        let mut field = PheromoneField::default().with_exploration(0.0);
        for t in 0..20 {
            field.record_outcome(&route("good"), true, 200.0, t);
            field.record_outcome(&route("bad"), false, 0.0, t);
        }
        let routes = [
            (route("bad"), 1.0),
            (route("good"), 1.0),
            (route("new"), 1.0),
        ];
        let mut good_first = 0;
        for seed in 0..200 {
            if field.rank(&routes, 20, seed)[0] == route("good") {
                good_first += 1;
            }
        }
        assert!(good_first > 150, "good route led {good_first}/200 times");
        let ranked = field.rank(&routes, 20, 1);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked.last().unwrap(), &route("bad"));
    }

    #[test]
    fn deposits_are_bounded_and_reject_garbage() {
        let mut field = PheromoneField::default();
        field.deposit(route("a"), PheromoneKind::Trail, f64::INFINITY, 0);
        field.deposit(route("a"), PheromoneKind::Trail, f64::NAN, 0);
        field.deposit(route("a"), PheromoneKind::Trail, -5.0, 0);
        assert_eq!(field.intensity(&route("a"), PheromoneKind::Trail, 0), 0.0);
        for _ in 0..1_000 {
            field.deposit(route("a"), PheromoneKind::Trail, 10.0, 0);
        }
        assert!(field.intensity(&route("a"), PheromoneKind::Trail, 0) <= MAX_INTENSITY);
    }
}
