// Output layer: payload structs and the multi-endpoint publisher, with a
// per-endpoint circuit breaker and a last-good-value cache.

use crate::adapter::Metrics;
use log::warn;
use reqwest::Client;
use serde::Serialize;
use std::time::{Duration, Instant};

pub struct CircuitBreaker {
    threshold: u32,
    breaker_dur: Duration,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, breaker_dur: Duration) -> Self {
        Self { threshold, breaker_dur, consecutive_failures: 0, opened_at: None }
    }
    /// True if a POST should be attempted now (closed or half-open).
    pub fn allow(&self) -> bool {
        match self.opened_at {
            None => true,
            Some(t) => t.elapsed() >= self.breaker_dur,
        }
    }
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.opened_at = None;
    }
    pub fn record_failure(&mut self) {
        if self.opened_at.is_some() {
            // half-open failure -> re-arm
            self.opened_at = Some(Instant::now());
            return;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.threshold {
            self.opened_at = Some(Instant::now());
            log::error!("circuit opened after {} consecutive failures", self.consecutive_failures);
        }
    }
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct BatteryData {
    #[serde(skip_serializing_if = "Option::is_none")] pub battery_level_percentage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub external_temp_celsius: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub battery_capacity_wh: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub battery_level_wh: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub battery_state_of_health: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub battery_voltage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub battery_current: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub interior_temp_celsius: Option<f32>,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct OdometerData {
    pub odometer_km: f32,
    #[serde(skip_serializing_if = "Option::is_none")] pub trip_km: Option<f32>,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct TirePressureData {
    pub pressures_kpa: Vec<f32>,
}

#[derive(Default, Clone)]
pub struct PublishCache {
    battery:        Option<BatteryData>,
    odometer:       Option<OdometerData>,
    tire_pressure:  Option<TirePressureData>,
}

impl PublishCache {
    pub fn last_battery(&self)       -> Option<&BatteryData>      { self.battery.as_ref() }
    pub fn last_odometer(&self)      -> Option<&OdometerData>     { self.odometer.as_ref() }
    pub fn last_tire_pressure(&self) -> Option<&TirePressureData> { self.tire_pressure.as_ref() }
    pub fn store_battery(&mut self, b: BatteryData)            { self.battery = Some(b); }
    pub fn store_odometer(&mut self, o: OdometerData)          { self.odometer = Some(o); }
    pub fn store_tire_pressure(&mut self, t: TirePressureData) { self.tire_pressure = Some(t); }
    pub fn clear(&mut self) { *self = PublishCache::default(); }
}

pub struct Publisher {
    client: Client,
    api_base: String,
    capacity_wh: Option<u32>,
    breakers: tokio::sync::Mutex<Breakers>,
    cache: tokio::sync::Mutex<PublishCache>,
    bridge_dropouts: bool,
}

struct Breakers {
    battery:       CircuitBreaker,
    odometer:      CircuitBreaker,
    tire_pressure: CircuitBreaker,
}

impl Publisher {
    pub fn new(
        api_base: impl Into<String>,
        capacity_wh: Option<u32>,
        threshold: u32,
        breaker_secs: u64,
        bridge_dropouts: bool,
    ) -> Self {
        let dur = Duration::from_secs(breaker_secs);
        Self {
            client: Client::new(),
            api_base: api_base.into(),
            capacity_wh,
            breakers: tokio::sync::Mutex::new(Breakers {
                battery:       CircuitBreaker::new(threshold, dur),
                odometer:      CircuitBreaker::new(threshold, dur),
                tire_pressure: CircuitBreaker::new(threshold, dur),
            }),
            cache: tokio::sync::Mutex::new(PublishCache::default()),
            bridge_dropouts,
        }
    }

    /// Called by the scheduler on AdapterError::Sleeping — drop stale telemetry.
    pub async fn on_sleeping(&self) {
        self.cache.lock().await.clear();
    }

    pub fn build_battery(&self, m: &Metrics) -> Option<BatteryData> {
        let relevant = ["battery_level_percentage","external_temp_celsius","battery_level_wh",
            "battery_state_of_health","battery_voltage","battery_current","interior_temp_celsius"];
        if !relevant.iter().any(|k| m.contains_key(*k)) { return None; }

        let derived_level_wh = match (m.get("battery_level_wh"), m.get("battery_level_percentage"), self.capacity_wh) {
            (Some(&v), _, _)             => Some(v as u64),
            (None, Some(&pct), Some(c))  => Some(((pct / 100.0) * c as f32) as u64),
            _                            => None,
        };

        Some(BatteryData {
            battery_level_percentage: m.get("battery_level_percentage").copied(),
            external_temp_celsius:    m.get("external_temp_celsius").copied(),
            battery_capacity_wh:      self.capacity_wh,
            battery_level_wh:         derived_level_wh,
            battery_state_of_health:  m.get("battery_state_of_health").copied(),
            battery_voltage:          m.get("battery_voltage").copied(),
            battery_current:          m.get("battery_current").copied(),
            interior_temp_celsius:    m.get("interior_temp_celsius").copied(),
        })
    }

    pub fn build_odometer(&self, m: &Metrics) -> Option<OdometerData> {
        m.get("odometer_km").map(|&odo| OdometerData {
            odometer_km: odo,
            trip_km: m.get("trip_km").copied(),
        })
    }

    pub fn build_tire_pressure(&self, m: &Metrics) -> Option<TirePressureData> {
        if !m.contains_key("tire_fl_kpa") { return None; }
        Some(TirePressureData {
            pressures_kpa: vec![
                m.get("tire_fl_kpa").copied().unwrap_or(0.0),
                m.get("tire_fr_kpa").copied().unwrap_or(0.0),
                m.get("tire_rl_kpa").copied().unwrap_or(0.0),
                m.get("tire_rr_kpa").copied().unwrap_or(0.0),
            ],
        })
    }

    /// Publish all three endpoints. Returns true if at least one POST succeeded.
    pub async fn publish(&self, m: &Metrics) -> bool {
        let mut any_ok = false;

        let payload = match self.build_battery(m) {
            Some(b) => Some(b),
            None if self.bridge_dropouts => self.cache.lock().await.last_battery().cloned(),
            None => None,
        };
        if let Some(b) = payload {
            if self.attempt_battery(&b).await {
                any_ok = true;
                self.cache.lock().await.store_battery(b);
            }
        }

        let payload = match self.build_odometer(m) {
            Some(o) => Some(o),
            None if self.bridge_dropouts => self.cache.lock().await.last_odometer().cloned(),
            None => None,
        };
        if let Some(o) = payload {
            if self.attempt_odometer(&o).await {
                any_ok = true;
                self.cache.lock().await.store_odometer(o);
            }
        }

        let payload = match self.build_tire_pressure(m) {
            Some(t) => Some(t),
            None if self.bridge_dropouts => self.cache.lock().await.last_tire_pressure().cloned(),
            None => None,
        };
        if let Some(t) = payload {
            if self.attempt_tire_pressure(&t).await {
                any_ok = true;
                self.cache.lock().await.store_tire_pressure(t);
            }
        }

        any_ok
    }

    async fn attempt_battery(&self, b: &BatteryData) -> bool {
        { if !self.breakers.lock().await.battery.allow() { return false; } }
        let ok = self.post(&format!("{}/battery", self.api_base), b).await;
        let mut br = self.breakers.lock().await;
        if ok { br.battery.record_success(); } else { br.battery.record_failure(); }
        ok
    }

    async fn attempt_odometer(&self, o: &OdometerData) -> bool {
        { if !self.breakers.lock().await.odometer.allow() { return false; } }
        let ok = self.post(&format!("{}/odometer", self.api_base), o).await;
        let mut br = self.breakers.lock().await;
        if ok { br.odometer.record_success(); } else { br.odometer.record_failure(); }
        ok
    }

    async fn attempt_tire_pressure(&self, t: &TirePressureData) -> bool {
        { if !self.breakers.lock().await.tire_pressure.allow() { return false; } }
        let ok = self.post(&format!("{}/tire-pressure", self.api_base), t).await;
        let mut br = self.breakers.lock().await;
        if ok { br.tire_pressure.record_success(); } else { br.tire_pressure.record_failure(); }
        ok
    }

    async fn post<T: Serialize>(&self, url: &str, body: &T) -> bool {
        match self.client.post(url).json(body).send().await {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => { warn!("POST {url} -> {}", r.status()); false }
            Err(e) => { warn!("POST {url}: {e}"); false }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn metrics_with(pairs: &[(&str, f32)]) -> Metrics {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn breaker_opens_after_threshold_failures() {
        let mut b = CircuitBreaker::new(3, Duration::from_millis(50));
        assert!(b.allow());
        b.record_failure();
        assert!(b.allow());
        b.record_failure();
        assert!(b.allow());
        b.record_failure(); // hits threshold
        assert!(!b.allow(), "should be open");
    }

    #[test]
    fn breaker_half_opens_after_timeout_and_closes_on_success() {
        let mut b = CircuitBreaker::new(1, Duration::from_millis(10));
        b.record_failure();
        assert!(!b.allow());
        std::thread::sleep(Duration::from_millis(20));
        assert!(b.allow(), "should be half-open");
        b.record_success();
        assert!(b.allow());
    }

    #[test]
    fn breaker_reopens_on_half_open_failure() {
        let mut b = CircuitBreaker::new(1, Duration::from_millis(10));
        b.record_failure();
        std::thread::sleep(Duration::from_millis(20));
        assert!(b.allow()); // half-open
        b.record_failure();
        assert!(!b.allow(), "should be re-opened");
    }

    #[test]
    fn build_battery_omits_when_no_relevant_fields() {
        let p = Publisher::new("http://test", None, 5, 300, true);
        assert!(p.build_battery(&Metrics::new()).is_none());
        assert!(p.build_battery(&metrics_with(&[("odometer_km", 12345.0)])).is_none());
    }

    #[test]
    fn build_battery_derives_level_wh_from_soc_and_capacity() {
        let p = Publisher::new("http://test", Some(80000), 5, 300, true);
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        let d = p.build_battery(&m).unwrap();
        assert_eq!(d.battery_level_percentage, Some(50.0));
        assert_eq!(d.battery_level_wh, Some(40000));
        assert_eq!(d.battery_capacity_wh, Some(80000));
    }

    #[test]
    fn build_battery_prefers_explicit_level_wh_over_derived() {
        let p = Publisher::new("http://test", Some(80000), 5, 300, true);
        let m = metrics_with(&[
            ("battery_level_percentage", 50.0),
            ("battery_level_wh", 12345.0),
        ]);
        let d = p.build_battery(&m).unwrap();
        assert_eq!(d.battery_level_wh, Some(12345));
    }

    #[test]
    fn build_odometer_present_only_when_field_present() {
        let p = Publisher::new("http://test", None, 5, 300, true);
        assert!(p.build_odometer(&Metrics::new()).is_none());
        let m = metrics_with(&[("odometer_km", 12345.0)]);
        let d = p.build_odometer(&m).unwrap();
        assert_eq!(d.odometer_km, 12345.0);
    }

    #[test]
    fn build_tire_pressure_requires_fl() {
        let p = Publisher::new("http://test", None, 5, 300, true);
        let m = metrics_with(&[("tire_fr_kpa", 200.0)]);
        assert!(p.build_tire_pressure(&m).is_none());

        let m = metrics_with(&[("tire_fl_kpa", 220.0), ("tire_rr_kpa", 210.0)]);
        let d = p.build_tire_pressure(&m).unwrap();
        assert_eq!(d.pressures_kpa, vec![220.0, 0.0, 0.0, 210.0]);
    }

    #[tokio::test]
    async fn publish_against_wiremock_hits_battery_endpoint() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server).await;

        let p = Publisher::new(server.uri(), Some(80000), 5, 300, true);
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        let ok = p.publish(&m).await;
        assert!(ok, "expected /battery POST to succeed");
    }

    #[tokio::test]
    async fn breaker_opens_after_repeated_500s() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server).await;

        let p = Publisher::new(server.uri(), Some(80000), 3, 60, true);
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        for _ in 0..3 { assert!(!p.publish(&m).await); } // 3 failures open the breaker
        assert!(!p.publish(&m).await);                    // open -> no POST
        let total = server.received_requests().await.unwrap().len();
        assert_eq!(total, 3, "publish should not POST after the circuit opens");
    }

    #[test]
    fn cache_remembers_last_good_payload() {
        let mut c = PublishCache::default();
        let bd = BatteryData { battery_level_percentage: Some(50.0), ..Default::default() };
        c.store_battery(bd.clone());
        assert!(matches!(c.last_battery(), Some(b) if b.battery_level_percentage == Some(50.0)));
    }

    #[test]
    fn cache_clear_wipes_everything() {
        let mut c = PublishCache::default();
        c.store_battery(BatteryData { battery_level_percentage: Some(50.0), ..Default::default() });
        c.store_odometer(OdometerData { odometer_km: 12345.0, trip_km: None });
        c.clear();
        assert!(c.last_battery().is_none());
        assert!(c.last_odometer().is_none());
    }

    #[tokio::test]
    async fn bridge_dropouts_reposts_cached_battery() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let p = Publisher::new(server.uri(), Some(80000), 5, 300, true);
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        assert!(p.publish(&m).await);              // real data: POST + cache
        assert!(p.publish(&Metrics::new()).await); // empty: cache replay -> POST
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bridge_dropouts_off_means_silent_no_data_cycle() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let p = Publisher::new(server.uri(), Some(80000), 5, 300, false);
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        assert!(p.publish(&m).await);
        assert!(!p.publish(&Metrics::new()).await); // no replay
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
