use serde::{Deserialize, Serialize};

/// Swarm roles from the proof of concept, mapped onto mesh duties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Coordinates routing for its clients (no single global queen: block
    /// proposing rotates across validators).
    Queen,
    /// Serves models.
    Worker,
    /// Explores new routes and models (exploration share in routing).
    Scout,
    /// Validates: attests ledger blocks and reports reputation.
    Guard,
    /// Tracks affective context (valence) for memories.
    Nurse,
    /// Archives memories and serves sync to peers.
    Memory,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queen => "queen",
            Self::Worker => "worker",
            Self::Scout => "scout",
            Self::Guard => "guard",
            Self::Nurse => "nurse",
            Self::Memory => "memory",
        }
    }
}

/// Lifecycle states. A node is never "off", it only changes state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Dormant,
    Germinating,
    Active,
    Flowering,
    Reproducing,
    Resting,
    Damaged,
    Healing,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vitals {
    /// 0..=100, driven by request success and latency.
    pub health: f64,
    /// 0..=100, spare capacity.
    pub energy: f64,
    /// 0..=5, mirrors the ledger reputation (milli / 1000).
    pub reputation: f64,
}

impl Default for Vitals {
    fn default() -> Self {
        Self {
            health: 100.0,
            energy: 100.0,
            reputation: 1.0,
        }
    }
}

/// Measurements fed into one lifecycle tick.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Signals {
    pub successes: u32,
    pub failures: u32,
    /// Mean latency of the window in milliseconds, if any requests ran.
    pub mean_latency_ms: Option<f64>,
    /// Latency considered healthy for this node's workload.
    pub target_latency_ms: f64,
    /// Requests in flight vs. what the node can take.
    pub in_flight: u32,
    pub capacity: u32,
    /// Ledger reputation in milli-units, when known.
    pub reputation_milli: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lifecycle {
    pub role: Role,
    pub state: NodeState,
    pub vitals: Vitals,
    pub born_ms: u64,
    pub last_transition_ms: u64,
}

impl Lifecycle {
    pub fn new(role: Role, now_ms: u64) -> Self {
        Self {
            role,
            state: NodeState::Germinating,
            vitals: Vitals::default(),
            born_ms: now_ms,
            last_transition_ms: now_ms,
        }
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.born_ms)
    }

    /// Fold a window of measurements into the vitals and step the state
    /// machine. Returns the new state if it changed.
    pub fn tick(&mut self, signals: &Signals, now_ms: u64) -> Option<NodeState> {
        self.update_vitals(signals);
        let next = self.next_state();
        if next == self.state {
            return None;
        }
        self.state = next;
        self.last_transition_ms = now_ms;
        Some(next)
    }

    fn update_vitals(&mut self, signals: &Signals) {
        let total = signals.successes + signals.failures;
        if total > 0 {
            let success_rate = f64::from(signals.successes) / f64::from(total);
            let latency_factor = match signals.mean_latency_ms {
                Some(latency) if signals.target_latency_ms > 0.0 && latency > 0.0 => {
                    (signals.target_latency_ms / latency).min(1.0)
                }
                _ => 1.0,
            };
            let observed = 100.0 * success_rate * (0.5 + 0.5 * latency_factor);
            // Exponential moving average: one bad window damages, a streak of
            // bad windows is needed to put the node in DAMAGED.
            self.vitals.health = 0.7 * self.vitals.health + 0.3 * observed;
        } else {
            // Idle windows heal slowly.
            self.vitals.health = (self.vitals.health + 2.0).min(100.0);
        }
        if signals.capacity > 0 {
            let load =
                f64::from(signals.in_flight.min(signals.capacity)) / f64::from(signals.capacity);
            self.vitals.energy = 100.0 * (1.0 - load);
        }
        if let Some(milli) = signals.reputation_milli {
            self.vitals.reputation = (milli as f64 / 1_000.0).clamp(0.0, 5.0);
        }
        self.vitals.health = self.vitals.health.clamp(0.0, 100.0);
    }

    fn next_state(&self) -> NodeState {
        let Vitals {
            health,
            energy,
            reputation,
        } = self.vitals;
        match self.state {
            NodeState::Dormant | NodeState::Germinating => NodeState::Active,
            _ if health < 30.0 => NodeState::Damaged,
            NodeState::Damaged => NodeState::Healing,
            NodeState::Healing if health > 70.0 => NodeState::Active,
            NodeState::Healing => NodeState::Healing,
            _ if energy < 20.0 => NodeState::Resting,
            NodeState::Resting if energy > 50.0 => NodeState::Active,
            NodeState::Resting => NodeState::Resting,
            NodeState::Active if reputation > 2.0 && energy > 80.0 => NodeState::Flowering,
            NodeState::Flowering if reputation > 3.0 && health > 90.0 => NodeState::Reproducing,
            NodeState::Flowering if reputation <= 2.0 || energy <= 80.0 => NodeState::Active,
            NodeState::Reproducing => NodeState::Active,
            state => state,
        }
    }

    /// Whether this node should currently accept routed work.
    pub fn accepts_work(&self) -> bool {
        !matches!(self.state, NodeState::Damaged | NodeState::Dormant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(successes: u32, failures: u32) -> Signals {
        Signals {
            successes,
            failures,
            mean_latency_ms: Some(100.0),
            target_latency_ms: 200.0,
            in_flight: 1,
            capacity: 10,
            reputation_milli: None,
        }
    }

    #[test]
    fn germinates_then_damages_then_heals() {
        let mut life = Lifecycle::new(Role::Worker, 0);
        assert_eq!(life.tick(&window(10, 0), 1), Some(NodeState::Active));
        for t in 0..10 {
            life.tick(&window(0, 10), 2 + t);
        }
        assert!(matches!(
            life.state,
            NodeState::Damaged | NodeState::Healing
        ));
        assert!(!life.accepts_work() || life.state == NodeState::Healing);
        for t in 0..30 {
            life.tick(&window(10, 0), 20 + t);
        }
        assert_eq!(life.state, NodeState::Active);
    }

    #[test]
    fn high_reputation_and_idle_capacity_flowers_then_reproduces() {
        let mut life = Lifecycle::new(Role::Worker, 0);
        let mut signals = window(10, 0);
        signals.reputation_milli = Some(3_500);
        signals.in_flight = 0;
        life.tick(&signals, 1);
        assert_eq!(life.tick(&signals, 2), Some(NodeState::Flowering));
        assert_eq!(life.tick(&signals, 3), Some(NodeState::Reproducing));
        assert_eq!(life.tick(&signals, 4), Some(NodeState::Active));
    }

    #[test]
    fn saturation_rests() {
        let mut life = Lifecycle::new(Role::Worker, 0);
        life.tick(&window(1, 0), 1);
        let mut busy = window(1, 0);
        busy.in_flight = 10;
        assert_eq!(life.tick(&busy, 2), Some(NodeState::Resting));
    }
}
