//! HeartBeat — self-improving agent loop across process lifetimes.
//!
//! Stores a persistent task + round counter. On server startup, if an
//! active heartbeat is configured, creates a new run with the stored
//! prompt. After each successful Review round, increments the counter,
//! commits progress, and exits cleanly so the external launcher can
//! restart with the latest binary.
//!
//! State file: `.graph_harness_heartbeat.json`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

const STATE_FILE: &str = ".graph_harness_heartbeat.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartBeat {
    /// The driving prompt for the optimization loop.
    pub prompt: String,
    /// Total rounds to run.
    pub max_rounds: usize,
    /// Rounds completed so far.
    pub completed_rounds: usize,
    /// Whether heartbeat is active.
    pub active: bool,
    /// The current run ID (set after creating a run).
    pub current_run_id: Option<String>,
}

impl HeartBeat {
    pub fn load() -> Option<Self> {
        let path = PathBuf::from(STATE_FILE);
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str::<HeartBeat>(&json) {
                    Ok(hb) => {
                        if hb.active { return Some(hb); }
                    }
                    Err(e) => warn!(error = %e, "heartbeat: corrupt state, skipping"),
                },
                Err(e) => warn!(error = %e, "heartbeat: cannot read state"),
            }
        }
        None
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(STATE_FILE, json);
        }
    }

    /// Set a new heartbeat task.
    pub fn start(prompt: String, max_rounds: usize) -> Self {
        let hb = HeartBeat {
            prompt,
            max_rounds,
            completed_rounds: 0,
            active: true,
            current_run_id: None,
        };
        hb.save();
        hb
    }

    /// Mark one round complete. Returns true if more rounds remain.
    pub fn round_complete(&mut self) -> bool {
        self.completed_rounds += 1;
        self.current_run_id = None;
        if self.completed_rounds >= self.max_rounds {
            self.active = false;
            self.save();
            info!("heartbeat: all {} rounds complete; deactivating", self.max_rounds);
            false
        } else {
            self.save();
            info!(
                "heartbeat: round {}/{} complete; {} remaining",
                self.completed_rounds, self.max_rounds,
                self.max_rounds - self.completed_rounds
            );
            true
        }
    }

    /// Deactivate and clean up.
    pub fn cancel(&mut self) {
        self.active = false;
        self.current_run_id = None;
        self.save();
    }
}
