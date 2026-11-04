// Output layer: payload structs and the multi-endpoint publisher, with a
// per-endpoint circuit breaker and a last-good-value cache.

use crate::adapter::Metrics;
use log::warn;
use reqwest::Client;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::Instant;

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

/// Body of aa-proxy-rs's POST /raw-topic-data: `payload` is JSON text, which
/// aa-proxy-rs forwards to its websocket bus as the latest value of `topic`.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct RawTopicData {
    pub topic: String,
    pub payload: String,
}

/// aa-proxy-rs ignores raw-topic payloads larger than this.
const MAX_RAW_PAYLOAD_BYTES: usize = 4096;

/// Shortest replay limit: a cached payload is replayed only while it is no older than
/// max(this, 2 x poll interval).
pub const MIN_REPLAY_AGE: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Stamped<T> {
    value: T,
    stored_at: Instant,
}

impl<T> Stamped<T> {
    fn new(value: T) -> Self { Self { value, stored_at: Instant::now() } }
}

/// Returns a clone of the cached value if it is still fresh; drops it otherwise.
fn take_fresh<T: Clone>(slot: &mut Option<Stamped<T>>, max_age: Duration) -> Option<T> {
    match slot {
        Some(s) if s.stored_at.elapsed() <= max_age => Some(s.value.clone()),
        Some(_) => { *slot = None; None }
        None => None,
    }
}

#[derive(Default, Clone)]
pub struct PublishCache {
    battery:        Option<Stamped<BatteryData>>,
    odometer:       Option<Stamped<OdometerData>>,
    tire_pressure:  Option<Stamped<TirePressureData>>,
}

impl PublishCache {
    #[cfg(test)]
    pub fn last_battery(&self)       -> Option<&BatteryData>      { self.battery.as_ref().map(|s| &s.value) }
    #[cfg(test)]
    pub fn last_odometer(&self)      -> Option<&OdometerData>     { self.odometer.as_ref().map(|s| &s.value) }
    pub fn store_battery(&mut self, b: BatteryData)            { self.battery = Some(Stamped::new(b)); }
    pub fn store_odometer(&mut self, o: OdometerData)          { self.odometer = Some(Stamped::new(o)); }
    pub fn store_tire_pressure(&mut self, t: TirePressureData) { self.tire_pressure = Some(Stamped::new(t)); }
    /// Replay candidates: a payload older than `max_age` is dropped, not returned.
    /// Reading never refreshes the stored time.
    pub fn fresh_battery(&mut self, max_age: Duration)       -> Option<BatteryData>      { take_fresh(&mut self.battery, max_age) }
    pub fn fresh_odometer(&mut self, max_age: Duration)      -> Option<OdometerData>     { take_fresh(&mut self.odometer, max_age) }
    pub fn fresh_tire_pressure(&mut self, max_age: Duration) -> Option<TirePressureData> { take_fresh(&mut self.tire_pressure, max_age) }
    pub fn clear(&mut self) { *self = PublishCache::default(); }
}

/// Converts a sensor value to an integer; non-finite and negative values yield None.
fn sensor_to_u64(v: f32) -> Option<u64> {
    if v.is_finite() && v >= 0.0 { Some(v as u64) } else { None }
}

/// State of charge (percent) of capacity (Wh); None unless both are finite and non-negative.
fn derive_level_wh(pct: f32, capacity_wh: f32) -> Option<u64> {
    if !pct.is_finite() || pct < 0.0 || !capacity_wh.is_finite() || capacity_wh < 0.0 {
        return None;
    }
    sensor_to_u64((pct / 100.0) * capacity_wh)
}

pub struct Publisher {
    client: Client,
    api_base: String,
    capacity_wh: Option<u32>,
    breakers: tokio::sync::Mutex<Breakers>,
    cache: tokio::sync::Mutex<PublishCache>,
    bridge_dropouts: bool,
    replay_limit: Duration,
    raw_topic: Option<String>,
}

struct Breakers {
    battery:       CircuitBreaker,
    odometer:      CircuitBreaker,
    tire_pressure: CircuitBreaker,
    raw_topic:     CircuitBreaker,
}

impl Publisher {
    pub fn new(
        api_base: impl Into<String>,
        capacity_wh: Option<u32>,
        threshold: u32,
        breaker_secs: u64,
        bridge_dropouts: bool,
    ) -> Result<Self, reqwest::Error> {
        Self::with_timeouts(api_base, capacity_wh, threshold, breaker_secs, bridge_dropouts,
            CONNECT_TIMEOUT, REQUEST_TIMEOUT)
    }

    pub(crate) fn with_timeouts(
        api_base: impl Into<String>,
        capacity_wh: Option<u32>,
        threshold: u32,
        breaker_secs: u64,
        bridge_dropouts: bool,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let dur = Duration::from_secs(breaker_secs);
        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            client,
            api_base: api_base.into(),
            capacity_wh,
            breakers: tokio::sync::Mutex::new(Breakers {
                battery:       CircuitBreaker::new(threshold, dur),
                odometer:      CircuitBreaker::new(threshold, dur),
                tire_pressure: CircuitBreaker::new(threshold, dur),
                raw_topic:     CircuitBreaker::new(threshold, dur),
            }),
            cache: tokio::sync::Mutex::new(PublishCache::default()),
            bridge_dropouts,
            replay_limit: MIN_REPLAY_AGE,
            raw_topic: None,
        })
    }

    /// Sends every poll cycle's metrics to POST /raw-topic-data under `topic`;
    /// `None` sends nothing there.
    pub fn with_raw_topic(mut self, topic: Option<String>) -> Self {
        self.raw_topic = topic;
        self
    }

    /// Sets the replay limit to max(`MIN_REPLAY_AGE`, 2 x `poll_interval`), so a
    /// slow poll cadence does not make every cached payload expire before reuse.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.replay_limit = poll_interval.saturating_mul(2).max(MIN_REPLAY_AGE);
        self
    }

    /// Called by the scheduler on AdapterError::Sleeping — drop stale telemetry.
    pub async fn on_sleeping(&self) {
        self.cache.lock().await.clear();
    }

    pub fn build_battery(&self, m: &Metrics) -> Option<BatteryData> {
        let relevant = ["battery_level_percentage","external_temp_celsius","battery_level_wh",
            "battery_state_of_health","battery_voltage","battery_current","interior_temp_celsius"];
        if !relevant.iter().any(|k| m.contains_key(*k)) { return None; }

        let explicit_wh = m.get("battery_level_wh").and_then(|&v| sensor_to_u64(v));
        let derived_level_wh = explicit_wh.or_else(|| match (m.get("battery_level_percentage"), self.capacity_wh) {
            (Some(&pct), Some(c)) => derive_level_wh(pct, c as f32),
            _                     => None,
        });
        // aa-proxy-rs rejects a battery payload with neither a percentage nor Wh.
        if !m.contains_key("battery_level_percentage") && derived_level_wh.is_none() {
            return None;
        }

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

    /// All finite metrics of one cycle as a JSON object with sorted keys, under
    /// the configured topic. None without a topic, without data, or when the
    /// payload exceeds what aa-proxy-rs accepts.
    pub fn build_raw_topic(&self, m: &Metrics) -> Option<RawTopicData> {
        let topic = self.raw_topic.as_ref()?;
        let values: BTreeMap<&str, f32> = m.iter()
            .filter(|(_, v)| v.is_finite())
            .map(|(k, &v)| (k.as_str(), v))
            .collect();
        if values.is_empty() { return None; }
        let payload = serde_json::to_string(&values).ok()?;
        if payload.len() > MAX_RAW_PAYLOAD_BYTES {
            warn!("raw topic payload is {} bytes, over the {MAX_RAW_PAYLOAD_BYTES}-byte limit; not sent",
                payload.len());
            return None;
        }
        Some(RawTopicData { topic: topic.clone(), payload })
    }

    /// Publish every endpoint that has data. Returns true if at least one POST succeeded.
    pub async fn publish(&self, m: &Metrics) -> bool {
        let mut any_ok = false;

        let (payload, replayed) = match self.build_battery(m) {
            Some(b) => (Some(b), false),
            None if self.bridge_dropouts => (self.cache.lock().await.fresh_battery(self.replay_limit), true),
            None => (None, false),
        };
        if let Some(b) = payload {
            if self.attempt_battery(&b).await {
                any_ok = true;
                if !replayed {
                    self.cache.lock().await.store_battery(b);
                }
            }
        }

        let (payload, replayed) = match self.build_odometer(m) {
            Some(o) => (Some(o), false),
            None if self.bridge_dropouts => (self.cache.lock().await.fresh_odometer(self.replay_limit), true),
            None => (None, false),
        };
        if let Some(o) = payload {
            if self.attempt_odometer(&o).await {
                any_ok = true;
                if !replayed {
                    self.cache.lock().await.store_odometer(o);
                }
            }
        }

        let (payload, replayed) = match self.build_tire_pressure(m) {
            Some(t) => (Some(t), false),
            None if self.bridge_dropouts => (self.cache.lock().await.fresh_tire_pressure(self.replay_limit), true),
            None => (None, false),
        };
        if let Some(t) = payload {
            if self.attempt_tire_pressure(&t).await {
                any_ok = true;
                if !replayed {
                    self.cache.lock().await.store_tire_pressure(t);
                }
            }
        }

        // Not replayed from the cache: overlay values must reflect this cycle.
        if let Some(r) = self.build_raw_topic(m) {
            if self.attempt_raw_topic(&r).await {
                any_ok = true;
            }
        }

        any_ok
    }

    async fn attempt_battery(&self, b: &BatteryData) -> bool {
        if !self.breakers.lock().await.battery.allow() { return false; }
        let ok = self.post(&format!("{}/battery", self.api_base), b).await;
        let mut br = self.breakers.lock().await;
        if ok { br.battery.record_success(); } else { br.battery.record_failure(); }
        ok
    }

    async fn attempt_odometer(&self, o: &OdometerData) -> bool {
        if !self.breakers.lock().await.odometer.allow() { return false; }
        let ok = self.post(&format!("{}/odometer", self.api_base), o).await;
        let mut br = self.breakers.lock().await;
        if ok { br.odometer.record_success(); } else { br.odometer.record_failure(); }
        ok
    }

    async fn attempt_tire_pressure(&self, t: &TirePressureData) -> bool {
        if !self.breakers.lock().await.tire_pressure.allow() { return false; }
        let ok = self.post(&format!("{}/tire-pressure", self.api_base), t).await;
        let mut br = self.breakers.lock().await;
        if ok { br.tire_pressure.record_success(); } else { br.tire_pressure.record_failure(); }
        ok
    }

    async fn attempt_raw_topic(&self, r: &RawTopicData) -> bool {
        if !self.breakers.lock().await.raw_topic.allow() { return false; }
        let ok = self.post(&format!("{}/raw-topic-data", self.api_base), r).await;
        let mut br = self.breakers.lock().await;
        if ok { br.raw_topic.record_success(); } else { br.raw_topic.record_failure(); }
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
        let p = Publisher::new("http://test", None, 5, 300, true).unwrap();
        assert!(p.build_battery(&Metrics::new()).is_none());
        assert!(p.build_battery(&metrics_with(&[("odometer_km", 12345.0)])).is_none());
    }

    #[test]
    fn build_battery_derives_level_wh_from_soc_and_capacity() {
        let p = Publisher::new("http://test", Some(80000), 5, 300, true).unwrap();
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        let d = p.build_battery(&m).unwrap();
        assert_eq!(d.battery_level_percentage, Some(50.0));
        assert_eq!(d.battery_level_wh, Some(40000));
        assert_eq!(d.battery_capacity_wh, Some(80000));
    }

    #[test]
    fn build_battery_prefers_explicit_level_wh_over_derived() {
        let p = Publisher::new("http://test", Some(80000), 5, 300, true).unwrap();
        let m = metrics_with(&[
            ("battery_level_percentage", 50.0),
            ("battery_level_wh", 12345.0),
        ]);
        let d = p.build_battery(&m).unwrap();
        assert_eq!(d.battery_level_wh, Some(12345));
    }

    #[test]
    fn build_battery_needs_a_percentage_or_wh() {
        let p = Publisher::new("http://x", Some(80000), 5, 300, false).unwrap();
        assert!(p.build_battery(&metrics_with(&[("external_temp_celsius", 12.0)])).is_none());
        let d = p.build_battery(&metrics_with(&[("external_temp_celsius", 12.0), ("battery_level_percentage", 50.0)]))
            .expect("percentage present");
        assert_eq!(d.external_temp_celsius, Some(12.0));
        assert!(p.build_battery(&metrics_with(&[("battery_level_wh", 30000.0)])).is_some());
    }

    #[test]
    fn build_odometer_present_only_when_field_present() {
        let p = Publisher::new("http://test", None, 5, 300, true).unwrap();
        assert!(p.build_odometer(&Metrics::new()).is_none());
        let m = metrics_with(&[("odometer_km", 12345.0)]);
        let d = p.build_odometer(&m).unwrap();
        assert_eq!(d.odometer_km, 12345.0);
    }

    #[test]
    fn build_tire_pressure_requires_fl() {
        let p = Publisher::new("http://test", None, 5, 300, true).unwrap();
        let m = metrics_with(&[("tire_fr_kpa", 200.0)]);
        assert!(p.build_tire_pressure(&m).is_none());

        let m = metrics_with(&[("tire_fl_kpa", 220.0), ("tire_rr_kpa", 210.0)]);
        let d = p.build_tire_pressure(&m).unwrap();
        assert_eq!(d.pressures_kpa, vec![220.0, 0.0, 0.0, 210.0]);
    }

    #[test]
    fn build_raw_topic_sends_every_finite_metric_under_the_topic() {
        let p = Publisher::new("http://x", None, 5, 300, false).unwrap();
        let m = metrics_with(&[("rpm", 766.5), ("coolant_temp_c", 88.0), ("bad", f32::NAN)]);
        assert_eq!(p.build_raw_topic(&m), None, "no topic configured");

        let p = p.with_raw_topic(Some("obd".into()));
        assert_eq!(p.build_raw_topic(&m), Some(RawTopicData {
            topic: "obd".into(),
            payload: r#"{"coolant_temp_c":88.0,"rpm":766.5}"#.into(),
        }));
        assert_eq!(p.build_raw_topic(&Metrics::new()), None, "no data");

        let many: Metrics = (0..400).map(|i| (format!("metric_{i:04}"), 1.0)).collect();
        assert_eq!(p.build_raw_topic(&many), None, "payload over the aa-proxy-rs limit");
    }

    #[tokio::test]
    async fn publish_posts_metrics_to_raw_topic_data() {
        use wiremock::{matchers::{body_json, method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/raw-topic-data"))
            .and(body_json(serde_json::json!({"topic": "obd", "payload": "{\"rpm\":766.5}"})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server).await;

        let p = Publisher::new(server.uri(), None, 5, 300, true).unwrap()
            .with_raw_topic(Some("obd".into()));
        assert!(p.publish(&metrics_with(&[("rpm", 766.5)])).await);
    }

    #[tokio::test]
    async fn publish_against_wiremock_hits_battery_endpoint() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server).await;

        let p = Publisher::new(server.uri(), Some(80000), 5, 300, true).unwrap();
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

        let p = Publisher::new(server.uri(), Some(80000), 3, 60, true).unwrap();
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

        let p = Publisher::new(server.uri(), Some(80000), 5, 300, true).unwrap();
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        assert!(p.publish(&m).await);              // real data: POST + cache
        assert!(p.publish(&Metrics::new()).await); // empty: cache replay -> POST
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn slow_endpoint_times_out_and_counts_as_failure() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(15)))
            .mount(&server).await;

        let p = Publisher::with_timeouts(server.uri(), Some(80000), 1, 60, true,
            Duration::from_millis(200), Duration::from_millis(500)).unwrap();
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        let started = std::time::Instant::now();
        assert!(!p.publish(&m).await);
        assert!(started.elapsed() < Duration::from_secs(5), "publish must not wait for the slow endpoint");
        assert!(!p.breakers.lock().await.battery.allow(), "timeout should open the breaker (threshold 1)");
    }

    /// Backdates the cached battery payload so ageing needs no waiting.
    async fn backdate_battery(p: &Publisher, age: Duration) {
        let mut c = p.cache.lock().await;
        c.battery.as_mut().unwrap().stored_at = Instant::now().checked_sub(age).unwrap();
    }

    #[tokio::test]
    async fn cached_payload_is_replayed_until_max_age_then_dropped() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let p = Publisher::new(server.uri(), Some(80000), 5, 300, true).unwrap();
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);

        assert!(p.publish(&m).await);
        backdate_battery(&p, Duration::from_secs(599)).await;
        assert!(p.publish(&Metrics::new()).await, "599 s old payload is replayed");
        let age = p.cache.lock().await.battery.as_ref().unwrap().stored_at.elapsed();
        assert!(age >= Duration::from_secs(599), "replay must not refresh the age: {age:?}");

        backdate_battery(&p, Duration::from_secs(601)).await;
        assert!(!p.publish(&Metrics::new()).await, "601 s old payload is not replayed");
        assert!(p.cache.lock().await.last_battery().is_none(), "expired payload is dropped");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn replay_limit_scales_with_slow_poll_interval() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200)).mount(&server).await;
        let p = Publisher::new(server.uri(), Some(80000), 5, 300, true).unwrap()
            .with_poll_interval(Duration::from_secs(900));
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);

        assert!(p.publish(&m).await);
        backdate_battery(&p, Duration::from_secs(1000)).await;
        assert!(p.publish(&Metrics::new()).await, "1000 s old payload is replayed at a 900 s poll");
        backdate_battery(&p, Duration::from_secs(1801)).await;
        assert!(!p.publish(&Metrics::new()).await, "1801 s old payload is not replayed");
    }

    #[test]
    fn invalid_soc_or_capacity_yields_no_derived_wh() {
        let p = Publisher::new("http://test", Some(80000), 5, 300, true).unwrap();
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            let d = p.build_battery(&metrics_with(&[("battery_level_percentage", bad)])).unwrap();
            assert_eq!(d.battery_level_wh, None, "soc {bad}");
            assert!(p.build_battery(&metrics_with(&[("battery_level_wh", bad)])).is_none(), "explicit wh {bad} alone");
        }
        let d = p.build_battery(&metrics_with(&[("battery_level_wh", f32::NAN), ("battery_level_percentage", 50.0)])).unwrap();
        assert_eq!(d.battery_level_wh, Some(40000), "invalid explicit wh falls back to the SoC-derived value");
        assert_eq!(derive_level_wh(50.0, -80000.0), None);
        assert_eq!(derive_level_wh(50.0, f32::NAN), None);
        assert_eq!(derive_level_wh(50.0, 80000.0), Some(40000));
    }

    #[tokio::test]
    async fn bridge_dropouts_off_means_silent_no_data_cycle() {
        use wiremock::{matchers::{method, path}, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/battery"))
            .respond_with(ResponseTemplate::new(200)).mount(&server).await;

        let p = Publisher::new(server.uri(), Some(80000), 5, 300, false).unwrap();
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        assert!(p.publish(&m).await);
        assert!(!p.publish(&Metrics::new()).await); // no replay
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
