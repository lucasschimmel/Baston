//! Pure adaptive-rate controller for authoritative sync scheduling.
//!
//! The controller makes one decision per completed tick. Scheduling remains
//! the caller's responsibility; in particular callers must skip missed
//! deadlines instead of replaying them in a burst.

use std::time::Duration;

const SAFE_RATES_HZ: &[u16] = &[20, 30, 40, 60, 90, 120];
const EWMA_ALPHA: f64 = 0.125;

#[derive(Debug, Clone, Copy)]
pub struct AdaptiveTickConfig {
    pub enabled: bool,
    pub min_hz: u16,
    pub initial_hz: u16,
    pub max_hz: u16,
    pub low_utilization: f64,
    pub high_utilization: f64,
    pub recovery_window: u32,
    pub overload_backoff: f64,
}

impl Default for AdaptiveTickConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_hz: 20,
            initial_hz: 60,
            max_hz: 120,
            low_utilization: 0.50,
            high_utilization: 0.85,
            recovery_window: 180,
            overload_backoff: 0.5,
        }
    }
}

impl From<&baston_config::StateSyncConfig> for AdaptiveTickConfig {
    fn from(value: &baston_config::StateSyncConfig) -> Self {
        Self {
            enabled: value.adaptive_tick_enabled,
            min_hz: value.tick_min_hz,
            initial_hz: value.tick_default_hz,
            max_hz: value.tick_max_hz,
            low_utilization: f64::from(value.tick_low_utilization),
            high_utilization: f64::from(value.tick_high_utilization),
            recovery_window: value.tick_recovery_window,
            overload_backoff: f64::from(value.tick_overload_backoff),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickRateReason {
    Disabled,
    Stable,
    SustainedHeadroom,
    WorkOverload,
    QueuePressure,
    DeadlineMiss,
}

#[derive(Debug, Clone, Copy)]
pub struct TickRateDecision {
    pub previous_hz: u16,
    pub hz: u16,
    pub utilization: f64,
    pub changed: bool,
    pub reason: TickRateReason,
}

#[derive(Debug)]
pub struct AdaptiveTickController {
    cfg: AdaptiveTickConfig,
    current_hz: u16,
    ewma_utilization: Option<f64>,
    headroom_samples: u32,
}

impl AdaptiveTickController {
    pub fn new(cfg: AdaptiveTickConfig) -> Self {
        let current_hz = nearest_rate(cfg.initial_hz, cfg.min_hz, cfg.max_hz);
        Self {
            cfg,
            current_hz,
            ewma_utilization: None,
            headroom_samples: 0,
        }
    }

    pub fn current_hz(&self) -> u16 {
        self.current_hz
    }

    /// Smoothed fraction of the scheduled period recent ticks consumed.
    ///
    /// `None` before the first observation: there is nothing to average yet,
    /// and reporting 0 would read as "idle" rather than "unknown".
    pub fn utilization(&self) -> Option<f64> {
        self.ewma_utilization
    }

    pub fn period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.current_hz))
    }

    /// Observe the work performed during one scheduled period.
    ///
    /// `queue_pressure` is normalized to `[0, 1]`. A full sync queue backs off
    /// immediately even if CPU utilization is low, protecting bandwidth and
    /// the reliable control plane.
    pub fn observe(
        &mut self,
        work: Duration,
        queue_pressure: f64,
        missed_deadline: bool,
    ) -> TickRateDecision {
        let previous_hz = self.current_hz;
        let sample = work.as_secs_f64() / self.period().as_secs_f64();
        let utilization = match self.ewma_utilization {
            Some(previous) => previous + EWMA_ALPHA * (sample - previous),
            None => sample,
        };
        self.ewma_utilization = Some(utilization);

        if !self.cfg.enabled {
            return self.decision(previous_hz, utilization, TickRateReason::Disabled);
        }

        let pressure = queue_pressure.clamp(0.0, 1.0);
        let overload_reason = if missed_deadline {
            Some(TickRateReason::DeadlineMiss)
        } else if pressure >= self.cfg.high_utilization {
            Some(TickRateReason::QueuePressure)
        } else if sample >= 1.0 || utilization >= self.cfg.high_utilization {
            Some(TickRateReason::WorkOverload)
        } else {
            None
        };

        if let Some(reason) = overload_reason {
            self.headroom_samples = 0;
            let target = (f64::from(self.current_hz) * self.cfg.overload_backoff) as u16;
            self.current_hz = lower_rate(target, self.cfg.min_hz, self.cfg.max_hz);
            return self.decision(previous_hz, utilization, reason);
        }

        if utilization <= self.cfg.low_utilization && pressure <= self.cfg.low_utilization {
            self.headroom_samples = self.headroom_samples.saturating_add(1);
            if self.headroom_samples >= self.cfg.recovery_window {
                self.headroom_samples = 0;
                self.current_hz = next_rate(self.current_hz, self.cfg.min_hz, self.cfg.max_hz);
                return self.decision(previous_hz, utilization, TickRateReason::SustainedHeadroom);
            }
        } else {
            self.headroom_samples = 0;
        }

        self.decision(previous_hz, utilization, TickRateReason::Stable)
    }

    fn decision(
        &self,
        previous_hz: u16,
        utilization: f64,
        reason: TickRateReason,
    ) -> TickRateDecision {
        TickRateDecision {
            previous_hz,
            hz: self.current_hz,
            utilization,
            changed: previous_hz != self.current_hz,
            reason,
        }
    }
}

fn rates(min_hz: u16, max_hz: u16) -> impl Iterator<Item = u16> {
    SAFE_RATES_HZ
        .iter()
        .copied()
        .filter(move |hz| *hz >= min_hz && *hz <= max_hz)
}

fn nearest_rate(requested: u16, min_hz: u16, max_hz: u16) -> u16 {
    rates(min_hz, max_hz)
        .min_by_key(|hz| hz.abs_diff(requested))
        .unwrap_or(requested.clamp(min_hz, max_hz))
}

fn lower_rate(target: u16, min_hz: u16, max_hz: u16) -> u16 {
    rates(min_hz, max_hz)
        .filter(|hz| *hz <= target)
        .last()
        .unwrap_or(min_hz)
}

fn next_rate(current: u16, min_hz: u16, max_hz: u16) -> u16 {
    rates(min_hz, max_hz)
        .find(|hz| *hz > current)
        .unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AdaptiveTickConfig {
        AdaptiveTickConfig {
            recovery_window: 3,
            ..Default::default()
        }
    }

    #[test]
    fn climbs_slowly_to_120_after_sustained_headroom() {
        let mut controller = AdaptiveTickController::new(test_config());
        for expected in [90, 120] {
            for _ in 0..3 {
                controller.observe(Duration::from_millis(1), 0.0, false);
            }
            assert_eq!(controller.current_hz(), expected);
        }
        for _ in 0..10 {
            controller.observe(Duration::from_millis(1), 0.0, false);
        }
        assert_eq!(controller.current_hz(), 120);
    }

    #[test]
    fn overload_falls_back_immediately_and_never_below_floor() {
        let mut controller = AdaptiveTickController::new(AdaptiveTickConfig {
            initial_hz: 120,
            ..test_config()
        });
        let first = controller.observe(Duration::from_millis(10), 0.0, true);
        assert_eq!(first.hz, 60);
        assert_eq!(first.reason, TickRateReason::DeadlineMiss);
        controller.observe(Duration::from_millis(60), 1.0, false);
        assert_eq!(controller.current_hz(), 30);
        controller.observe(Duration::from_millis(60), 1.0, false);
        assert_eq!(controller.current_hz(), 20);
        controller.observe(Duration::from_millis(60), 1.0, false);
        assert_eq!(controller.current_hz(), 20);
    }

    #[test]
    fn intermittent_load_resets_recovery_hysteresis() {
        let mut controller = AdaptiveTickController::new(test_config());
        controller.observe(Duration::from_millis(1), 0.0, false);
        controller.observe(Duration::from_millis(1), 0.0, false);
        controller.observe(Duration::from_millis(12), 0.6, false);
        controller.observe(Duration::from_millis(1), 0.0, false);
        assert_eq!(controller.current_hz(), 60);
    }
}
