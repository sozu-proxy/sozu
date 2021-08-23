use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
  pub static FEATURES: RefCell<FeatureFlags> = RefCell::new(FeatureFlags::new());
}

/// accepting feature flags from env var with the format FEATURES=key;type;value,key;type;value
/// with type 'b' for boolean, 's' for string, 'i' for i64, value in the expected type
pub struct FeatureFlags {
    features: HashMap<String, Feature>,
}

impl FeatureFlags {
    pub fn new() -> FeatureFlags {
        let mut features = HashMap::new();
        if let Ok(val) = std::env::var("FEATURES") {
            for feature_val in val.split(",") {
                if let Some((key, f)) = parse_feature(feature_val) {
                    info!("adding feature flag ({}, {:?})", key, f);
                    features.insert(key, f);
                }
            }
        }

        FeatureFlags { features }
    }

    pub fn get(&self, key: &str) -> Option<&Feature> {
        self.features.get(key)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Feature {
    Boolean(bool),
    String(String),
    Number(i64),
}

impl Feature {
    pub fn is_true(&self) -> bool {
        self == &Feature::Boolean(true)
    }

    pub fn is_string(&self, s: &str) -> bool {
        if let Feature::String(ref s1) = self {
            s1 == s
        } else {
            false
        }
    }
}

fn parse_feature(s: &str) -> Option<(String, Feature)> {
    let v = s.split(";").collect::<Vec<_>>();
    if v.len() != 3 {
        return None;
    }

    let f = if v[1] == "b" {
        if let Some(b) = v[2].parse().ok() {
            Feature::Boolean(b)
        } else {
            return None;
        }
    } else if v[1] == "s" {
        Feature::String(v[2].to_string())
    } else if v[1] == "i" {
        if let Some(i) = v[2].parse().ok() {
            Feature::Number(i)
        } else {
            return None;
        }
    } else {
        return None;
    };

    return Some((v[0].to_string(), f));
}
