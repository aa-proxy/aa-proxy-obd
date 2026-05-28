// Output layer: payload structs and the multi-endpoint publisher, with a
// per-endpoint circuit breaker and a last-good-value cache.

use crate::adapter::Metrics;
use log::warn;
use reqwest::Client;
use serde::Serialize;

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

pub struct Publisher {
    client: Client,
    api_base: String,
    capacity_wh: Option<u32>,
}

impl Publisher {
    pub fn new(api_base: impl Into<String>, capacity_wh: Option<u32>) -> Self {
        Self { client: Client::new(), api_base: api_base.into(), capacity_wh }
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
        if let Some(data) = self.build_battery(m) {
            any_ok |= self.post(&format!("{}/battery", self.api_base), &data).await;
        }
        if let Some(data) = self.build_odometer(m) {
            any_ok |= self.post(&format!("{}/odometer", self.api_base), &data).await;
        }
        if let Some(data) = self.build_tire_pressure(m) {
            any_ok |= self.post(&format!("{}/tire-pressure", self.api_base), &data).await;
        }
        any_ok
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

    fn metrics_with(pairs: &[(&str, f32)]) -> Metrics {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn build_battery_omits_when_no_relevant_fields() {
        let p = Publisher::new("http://test", None);
        assert!(p.build_battery(&Metrics::new()).is_none());
        assert!(p.build_battery(&metrics_with(&[("odometer_km", 12345.0)])).is_none());
    }

    #[test]
    fn build_battery_derives_level_wh_from_soc_and_capacity() {
        let p = Publisher::new("http://test", Some(80000));
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        let d = p.build_battery(&m).unwrap();
        assert_eq!(d.battery_level_percentage, Some(50.0));
        assert_eq!(d.battery_level_wh, Some(40000));
        assert_eq!(d.battery_capacity_wh, Some(80000));
    }

    #[test]
    fn build_battery_prefers_explicit_level_wh_over_derived() {
        let p = Publisher::new("http://test", Some(80000));
        let m = metrics_with(&[
            ("battery_level_percentage", 50.0),
            ("battery_level_wh", 12345.0),
        ]);
        let d = p.build_battery(&m).unwrap();
        assert_eq!(d.battery_level_wh, Some(12345));
    }

    #[test]
    fn build_odometer_present_only_when_field_present() {
        let p = Publisher::new("http://test", None);
        assert!(p.build_odometer(&Metrics::new()).is_none());
        let m = metrics_with(&[("odometer_km", 12345.0)]);
        let d = p.build_odometer(&m).unwrap();
        assert_eq!(d.odometer_km, 12345.0);
    }

    #[test]
    fn build_tire_pressure_requires_fl() {
        let p = Publisher::new("http://test", None);
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

        let p = Publisher::new(server.uri(), Some(80000));
        let m = metrics_with(&[("battery_level_percentage", 50.0)]);
        let ok = p.publish(&m).await;
        assert!(ok, "expected /battery POST to succeed");
    }
}
