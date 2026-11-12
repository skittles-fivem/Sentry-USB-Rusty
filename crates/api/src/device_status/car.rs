//! The `car` object in `.sentryusb/device-status.json`: battery,
//! temperatures, tire pressures and parked or driving, from the Tesla BLE
//! telemetry the Pi already keeps. Built from the same in-process answers the
//! dashboard reads (`/api/system/ble-latest-sample`, `/api/charging/current`,
//! `/api/drives/stats`) and the owner's tire-band setting. Nothing else from
//! the sample (place, position, odometer, VIN) is written.
//!
//! In the file: temperatures in °C, pressures in kPa, times in Unix seconds.
//! Unknown readings are left out; with nothing known there is no `car`.

use serde_json::{json, Map, Value};

use sentryusb_config::SetupConfig;

/// 1 psi in kPa.
const KPA_PER_PSI: f64 = 6.894_757;
/// A gear reading older than this is not used (the dashboard's limit).
const SHIFT_FRESH_SECS: i64 = 120;

/// Tire bands in psi, as TIRE_PSI_BANDS holds them (the web UI's defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bands {
    pub unsafe_low: f64,
    pub optimal_low: f64,
    pub optimal_high: f64,
    pub unsafe_high: f64,
}

pub const DEFAULT_BANDS: Bands = Bands {
    unsafe_low: 28.0,
    optimal_low: 36.0,
    optimal_high: 45.0,
    unsafe_high: 50.0,
};

/// TIRE_PSI_BANDS ("unsafe-low,optimal-low,optimal-high,unsafe-high"), or
/// the defaults when unset or malformed, exactly as web/src/lib/tireBands.ts.
pub fn parse_bands(raw: Option<&str>) -> Bands {
    let parts: Vec<&str> = raw.unwrap_or("").split(',').map(str::trim).collect();
    if parts.len() != 4 || parts.iter().any(|s| s.is_empty()) {
        return DEFAULT_BANDS;
    }
    let v: Vec<f64> = parts
        .iter()
        .map(|s| s.parse::<f64>().unwrap_or(f64::NAN))
        .collect();
    let ok = v.iter().all(|n| n.is_finite() && (5.0..=150.0).contains(n))
        && v[0] < v[1]
        && v[1] < v[2]
        && v[2] < v[3];
    if !ok {
        return DEFAULT_BANDS;
    }
    Bands {
        unsafe_low: v[0],
        optimal_low: v[1],
        optimal_high: v[2],
        unsafe_high: v[3],
    }
}

/// "optimal", "check" or "unsafe" for the tires known, worst one wins.
pub fn tire_state(psi: &[f64], b: Bands) -> Option<&'static str> {
    if psi.is_empty() {
        return None;
    }
    let class = |p: f64| {
        if p < b.unsafe_low || p > b.unsafe_high {
            2
        } else if p < b.optimal_low || p > b.optimal_high {
            1
        } else {
            0
        }
    };
    Some(match psi.iter().map(|p| class(*p)).max().unwrap_or(0) {
        2 => "unsafe",
        1 => "check",
        _ => "optimal",
    })
}

/// What the car object is built from.
pub struct Inputs<'a> {
    /// GET /api/system/ble-latest-sample.
    pub sample: Option<&'a Value>,
    /// GET /api/charging/current.
    pub charge: Option<&'a Value>,
    /// The end of the latest drive, Unix seconds (from GET /api/drives/stats).
    pub last_drive_end: Option<i64>,
    /// The active lines of sentryusb.conf.
    pub config: &'a SetupConfig,
    pub now: i64,
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn age(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64).filter(|n| *n >= 0)
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

/// The `car` object, or None when no reading is known.
pub fn build(i: &Inputs) -> Option<Value> {
    let s = i
        .sample
        .filter(|s| s.get("ts").is_some_and(|t| !t.is_null()))?;
    let ages = s.get("field_secs_ago");
    let field_age = |k: &str| age(ages.and_then(|a| a.get(k)));
    // A reading's time: now minus its age, or the sample's own time.
    let sample_ts = s.get("ts").and_then(Value::as_i64);
    let at = |k: &str| field_age(k).map(|a| i.now - a).or(sample_ts);
    let mut car = Map::new();

    if let Some(pct) = finite(s.get("battery_pct")).filter(|p| (0.0..=100.0).contains(p)) {
        let mut b = json!({"pct": round1(pct)});
        if let Some(t) = at("battery_pct") {
            b["at"] = t.into();
        }
        if let Some(c) = i
            .charge
            .and_then(|c| c.get("charging"))
            .and_then(Value::as_bool)
        {
            b["charging"] = c.into();
        }
        car.insert("battery".into(), b);
    }
    for (key, field, range) in [
        ("interior", "interior_temp_c", -60.0..=100.0),
        ("exterior", "exterior_temp_c", -70.0..=70.0),
    ] {
        if let Some(c) = finite(s.get(field)).filter(|c| range.contains(c)) {
            let mut t = json!({"c": round1(c)});
            if let Some(a) = at(field) {
                t["at"] = a.into();
            }
            car.insert(key.into(), t);
        }
    }

    let bands = parse_bands(i.config.get("TIRE_PSI_BANDS").map(String::as_str));
    let mut tires = Map::new();
    let mut psis = Vec::new();
    for pos in ["fl", "fr", "rl", "rr"] {
        let Some(psi) =
            finite(s.get(format!("tire_{pos}_psi").as_str())).filter(|p| (0.0..=145.0).contains(p))
        else {
            continue;
        };
        psis.push(psi);
        let mut t = json!({"kpa": round1(psi * KPA_PER_PSI)});
        // Each tire's own age; older Pis sent only `tires` (front-left's).
        if let Some(a) = field_age(&format!("tire_{pos}"))
            .or_else(|| field_age("tires"))
            .map(|a| i.now - a)
        {
            t["at"] = a.into();
        }
        tires.insert(pos.into(), t);
    }
    if let Some(state) = tire_state(&psis, bands) {
        tires.insert("state".into(), state.into());
        car.insert("tires".into(), Value::Object(tires));
    }

    // Parked or driving: a fresh gear reading, else parked since the last drive ended.
    let shift = s.get("shift_state").and_then(Value::as_str);
    let shift_age = age(s.get("shift_state_seconds_ago")).filter(|a| *a <= SHIFT_FRESH_SECS);
    let drive = match (shift, shift_age) {
        (Some("Drive" | "Reverse" | "Neutral"), Some(a)) => {
            Some(json!({"state": "driving", "at": i.now - a}))
        }
        (Some("Park"), Some(a)) => {
            let mut d = json!({"state": "parked", "at": i.now - a});
            if let Some(end) = i.last_drive_end.filter(|e| *e <= i.now) {
                d["since"] = end.into();
            }
            Some(d)
        }
        _ => i
            .last_drive_end
            .filter(|e| *e <= i.now)
            .map(|end| json!({"state": "parked", "since": end})),
    };
    if let Some(d) = drive {
        car.insert("drive".into(), d);
    }

    if car.is_empty() {
        return None;
    }
    Some(Value::Object(car))
}

/// The car object without its reading times, to tell a change from a newer
/// reading of the same values.
pub fn signature(car: &Value) -> Value {
    fn strip(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| k.as_str() != "at")
                    .map(|(k, v)| (k.clone(), strip(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    strip(car)
}

/// The drive-stats `latest_drive_end` (local time, as the clips name it)
/// in Unix seconds.
pub fn parse_local_time(s: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    naive
        .and_local_timezone(chrono::Local)
        .earliest()
        .map(|t| t.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_762_800_000;

    fn cfg(pairs: &[(&str, &str)]) -> SetupConfig {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn sample() -> Value {
        json!({
            "ts": NOW - 60, "seconds_ago": 60,
            "battery_pct": 89.0, "interior_temp_c": 9.4, "exterior_temp_c": 7.8,
            "tire_fl_psi": 42.1, "tire_fr_psi": 42.0, "tire_rl_psi": 41.9, "tire_rr_psi": 42.3,
            "odometer_mi": 12345.0, "location_name": "Home", "at_home": true,
            "field_secs_ago": {"battery_pct": 60, "interior_temp_c": 60, "exterior_temp_c": 120,
                "tires": 7200, "tire_fl": 7200, "tire_fr": 7200, "tire_rl": 28800, "tire_rr": 28800},
            "shift_state": null, "shift_state_seconds_ago": 4000,
        })
    }

    #[test]
    fn builds_the_contract_object_and_nothing_else() {
        let s = sample();
        let charge = json!({"charging": false, "soc": 89.0});
        let c = cfg(&[("TEMPERATURE_UNIT", "F")]);
        let car = build(&Inputs {
            sample: Some(&s),
            charge: Some(&charge),
            last_drive_end: Some(NOW - 9000),
            config: &c,
            now: NOW,
        })
        .unwrap();
        assert_eq!(
            car,
            json!({
                "battery": {"pct": 89.0, "at": NOW - 60, "charging": false},
                "interior": {"c": 9.4, "at": NOW - 60},
                "exterior": {"c": 7.8, "at": NOW - 120},
                "tires": {
                    "fl": {"kpa": 290.3, "at": NOW - 7200}, "fr": {"kpa": 289.6, "at": NOW - 7200},
                    "rl": {"kpa": 288.9, "at": NOW - 28800}, "rr": {"kpa": 291.6, "at": NOW - 28800},
                    "state": "optimal",
                },
                "drive": {"state": "parked", "since": NOW - 9000},
            })
        );
        // never the place, position, odometer, VIN or display units
        let text = car.to_string();
        for k in ["location", "home", "odometer", "lat", "lon", "vin", "units"] {
            assert!(!text.contains(k), "{k} must not be sent");
        }
    }

    #[test]
    fn unknown_readings_are_left_out() {
        let c = cfg(&[]);
        let none = build(&Inputs {
            sample: Some(&json!({"ts": null})),
            charge: None,
            last_drive_end: Some(NOW - 10),
            config: &c,
            now: NOW,
        });
        assert_eq!(none, None, "no car data at all: no car object");
        assert_eq!(
            build(&Inputs {
                sample: None,
                charge: None,
                last_drive_end: None,
                config: &c,
                now: NOW
            }),
            None
        );
        let s = json!({"ts": NOW, "battery_pct": null, "interior_temp_c": 250.0, "exterior_temp_c": f64::NAN,
            "tire_fl_psi": 30.0, "tire_rr_psi": -3.0, "field_secs_ago": {"tires": 30}});
        let car = build(&Inputs {
            sample: Some(&s),
            charge: None,
            last_drive_end: None,
            config: &c,
            now: NOW,
        })
        .unwrap();
        assert_eq!(
            car,
            json!({
                "tires": {"fl": {"kpa": 206.8, "at": NOW - 30}, "state": "check"},
            })
        );
    }

    #[test]
    fn gear_decides_parked_or_driving() {
        let c = cfg(&[]);
        let mut s = sample();
        s["shift_state"] = "Drive".into();
        s["shift_state_seconds_ago"] = 5.into();
        let car = build(&Inputs {
            sample: Some(&s),
            charge: None,
            last_drive_end: Some(NOW - 9000),
            config: &c,
            now: NOW,
        })
        .unwrap();
        assert_eq!(car["drive"], json!({"state": "driving", "at": NOW - 5}));
        s["shift_state"] = "Park".into();
        let car = build(&Inputs {
            sample: Some(&s),
            charge: None,
            last_drive_end: Some(NOW - 9000),
            config: &c,
            now: NOW,
        })
        .unwrap();
        assert_eq!(
            car["drive"],
            json!({"state": "parked", "at": NOW - 5, "since": NOW - 9000})
        );
        // a stale gear and no drive yet: unknown
        s["shift_state_seconds_ago"] = 500.into();
        let car = build(&Inputs {
            sample: Some(&s),
            charge: None,
            last_drive_end: None,
            config: &c,
            now: NOW,
        })
        .unwrap();
        assert!(car.get("drive").is_none());
    }

    #[test]
    fn tire_state_uses_the_owners_bands() {
        assert_eq!(tire_state(&[], DEFAULT_BANDS), None);
        assert_eq!(tire_state(&[40.0, 41.0], DEFAULT_BANDS), Some("optimal"));
        assert_eq!(tire_state(&[40.0, 34.0], DEFAULT_BANDS), Some("check"));
        assert_eq!(tire_state(&[34.0, 27.0], DEFAULT_BANDS), Some("unsafe"));
        let b = parse_bands(Some("20, 30, 35, 40"));
        assert_eq!(
            b,
            Bands {
                unsafe_low: 20.0,
                optimal_low: 30.0,
                optimal_high: 35.0,
                unsafe_high: 40.0
            }
        );
        assert_eq!(tire_state(&[42.0], b), Some("unsafe"));
        for bad in [
            "",
            "1,2,3",
            "30,20,35,40",
            "a,b,c,d",
            "1,2,3,4",
            "36,,45,50",
        ] {
            assert_eq!(parse_bands(Some(bad)), DEFAULT_BANDS, "{bad}");
        }
    }

    #[test]
    fn signature_ignores_reading_times_only() {
        let a =
            json!({"battery": {"pct": 80.0, "at": 1}, "tires": {"fl": {"kpa": 290.0, "at": 5}}});
        let b =
            json!({"battery": {"pct": 80.0, "at": 2}, "tires": {"fl": {"kpa": 290.0, "at": 9}}});
        let c =
            json!({"battery": {"pct": 79.0, "at": 2}, "tires": {"fl": {"kpa": 290.0, "at": 9}}});
        assert_eq!(signature(&a), signature(&b));
        assert_ne!(signature(&a), signature(&c));
    }

    #[test]
    fn drive_end_is_local_time() {
        let t = parse_local_time("2026-11-10T08:30:00.000").unwrap();
        let back = chrono::DateTime::from_timestamp(t, 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(
            back.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "2026-11-10T08:30:00"
        );
        assert_eq!(parse_local_time("yesterday"), None);
    }
}
