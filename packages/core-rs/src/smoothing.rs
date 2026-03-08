use crate::mapping::{clamp, clamp01};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OutputTuning {
    pub emit_hz: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
    pub ema_alpha: f64,
    pub min_delta: f64,
    pub heartbeat_ms: u64,
}

impl Default for OutputTuning {
    fn default() -> Self {
        Self {
            emit_hz: 20.0,
            attack_ms: 55.0,
            release_ms: 220.0,
            ema_alpha: 0.35,
            min_delta: 0.015,
            heartbeat_ms: 1_000,
        }
    }
}

impl OutputTuning {
    pub fn sanitize(mut self) -> Self {
        self.emit_hz = clamp(self.emit_hz, 2.0, 60.0);
        self.attack_ms = clamp(self.attack_ms, 10.0, 2_000.0);
        self.release_ms = clamp(self.release_ms, 10.0, 5_000.0);
        self.ema_alpha = clamp(self.ema_alpha, 0.01, 1.0);
        self.min_delta = clamp(self.min_delta, 0.0, 1.0);
        self.heartbeat_ms = self.heartbeat_ms.clamp(200, 10_000);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmootherState {
    pub target_intensity: f64,
    pub current_intensity: f64,
    pub peak_intensity: f64,
    pub last_sent_intensity: f64,
    pub last_sent_at_ms: u64,
}

impl Default for SmootherState {
    fn default() -> Self {
        Self {
            target_intensity: 0.0,
            current_intensity: 0.0,
            peak_intensity: 0.0,
            last_sent_intensity: 0.0,
            last_sent_at_ms: 0,
        }
    }
}

impl SmootherState {
    pub fn step(&mut self, now_ms: u64, last_tick_ms: u64, tuning: OutputTuning) {
        let dt_ms = now_ms.saturating_sub(last_tick_ms).max(1) as f64;
        let target = clamp01(self.target_intensity);
        let current = self.current_intensity;
        let tau_ms = if target >= current {
            tuning.attack_ms
        } else {
            tuning.release_ms
        }
        .max(1.0);
        let lerp_alpha = 1.0 - (-dt_ms / tau_ms).exp();
        let mut next = current + (target - current) * lerp_alpha;
        next = tuning.ema_alpha * next + (1.0 - tuning.ema_alpha) * current;
        self.current_intensity = clamp01(next);

        if self.current_intensity >= self.peak_intensity {
            self.peak_intensity = self.current_intensity;
        } else {
            self.peak_intensity = self.current_intensity.max(self.peak_intensity - 0.025);
        }
    }

    pub fn should_emit(&self, now_ms: u64, tuning: OutputTuning) -> bool {
        let delta = (self.current_intensity - self.last_sent_intensity).abs();
        if delta >= tuning.min_delta {
            return true;
        }
        now_ms.saturating_sub(self.last_sent_at_ms) >= tuning.heartbeat_ms
    }

    pub fn mark_emitted(&mut self, now_ms: u64) {
        self.last_sent_intensity = self.current_intensity;
        self.last_sent_at_ms = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_uses_attack_constant_when_rising() {
        let tuning = OutputTuning {
            attack_ms: 50.0,
            release_ms: 220.0,
            ema_alpha: 1.0,
            ..OutputTuning::default()
        };
        let mut state = SmootherState {
            target_intensity: 1.0,
            current_intensity: 0.0,
            ..SmootherState::default()
        };
        state.step(100, 0, tuning);
        assert!((state.current_intensity - 0.8646647167).abs() < 1e-6);
    }

    #[test]
    fn step_uses_release_constant_when_falling() {
        let tuning = OutputTuning {
            attack_ms: 55.0,
            release_ms: 200.0,
            ema_alpha: 1.0,
            ..OutputTuning::default()
        };
        let mut state = SmootherState {
            target_intensity: 0.0,
            current_intensity: 1.0,
            peak_intensity: 1.0,
            ..SmootherState::default()
        };
        state.step(100, 0, tuning);
        assert!((state.current_intensity - 0.6065306597).abs() < 1e-6);
        assert!((state.peak_intensity - 0.975).abs() < 1e-9);
    }

    #[test]
    fn peak_tracks_and_then_decays() {
        let tuning = OutputTuning {
            ema_alpha: 1.0,
            attack_ms: 10.0,
            release_ms: 10.0,
            ..OutputTuning::default()
        };
        let mut state = SmootherState {
            target_intensity: 1.0,
            ..SmootherState::default()
        };
        state.step(10, 0, tuning);
        let peak_after_rise = state.peak_intensity;
        assert!(peak_after_rise > 0.0);

        state.target_intensity = 0.0;
        state.step(20, 10, tuning);
        assert!(state.peak_intensity <= peak_after_rise);
        assert!(state.peak_intensity >= state.current_intensity);
    }

    #[test]
    fn emit_gates_on_delta_or_heartbeat() {
        let tuning = OutputTuning {
            min_delta: 0.1,
            heartbeat_ms: 1_000,
            ..OutputTuning::default()
        };
        let mut state = SmootherState {
            current_intensity: 0.05,
            last_sent_intensity: 0.0,
            last_sent_at_ms: 100,
            ..SmootherState::default()
        };
        assert!(!state.should_emit(500, tuning));
        assert!(state.should_emit(1_200, tuning));

        state.current_intensity = 0.3;
        assert!(state.should_emit(1_201, tuning));
        state.mark_emitted(1_201);
        assert_eq!(state.last_sent_at_ms, 1_201);
        assert!((state.last_sent_intensity - 0.3).abs() < 1e-9);
    }
}
