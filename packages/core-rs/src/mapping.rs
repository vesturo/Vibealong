use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseInQuad,
    EaseInOutQuad,
}

impl Curve {
    pub fn from_name(value: &str) -> Self {
        match value.trim() {
            "easeOutQuad" => Self::EaseOutQuad,
            "easeInQuad" => Self::EaseInQuad,
            "easeInOutQuad" => Self::EaseInOutQuad,
            _ => Self::Linear,
        }
    }

    pub fn apply(self, value: f64) -> f64 {
        let x = clamp01(value);
        match self {
            Self::Linear => x,
            Self::EaseOutQuad => 1.0 - (1.0 - x) * (1.0 - x),
            Self::EaseInQuad => x * x,
            Self::EaseInOutQuad => {
                if x < 0.5 {
                    2.0 * x * x
                } else {
                    1.0 - ((-2.0 * x + 2.0).powf(2.0) / 2.0)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    pub address: String,
    pub weight: f64,
    pub deadzone: f64,
    pub invert: bool,
    pub curve: Curve,
    pub min: f64,
    pub max: f64,
}

impl Mapping {
    pub fn new(
        address: impl Into<String>,
        weight: f64,
        deadzone: f64,
        invert: bool,
        curve: Curve,
        min: f64,
        max: f64,
    ) -> Option<Self> {
        let address = address.into().trim().to_string();
        if !address.starts_with('/') {
            return None;
        }
        let deadzone = clamp(deadzone, 0.0, 1.0);
        let max = if max <= min { min + 1.0 } else { max };
        Some(Self {
            address,
            weight: if weight.is_finite() { weight } else { 1.0 },
            deadzone,
            invert,
            curve,
            min: if min.is_finite() { min } else { 0.0 },
            max,
        })
    }
}

pub fn clamp01(value: f64) -> f64 {
    clamp(value, 0.0, 1.0)
}

pub fn clamp(value: f64, min: f64, max: f64) -> f64 {
    if !value.is_finite() {
        return min;
    }
    if value < min {
        return min;
    }
    if value > max {
        return max;
    }
    value
}

pub fn map_input_value(raw_value: f64, mapping: &Mapping) -> f64 {
    let normalized = (raw_value - mapping.min) / (mapping.max - mapping.min);
    let mut value = clamp01(normalized);
    if mapping.invert {
        value = 1.0 - value;
    }
    if value < mapping.deadzone {
        value = 0.0;
    }
    value = mapping.curve.apply(value);
    clamp01(value)
}

pub fn calculate_composite_intensity(
    current_inputs: &HashMap<String, f64>,
    mappings: &[Mapping],
) -> f64 {
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;

    for mapping in mappings {
        let Some(sample) = current_inputs.get(&mapping.address) else {
            continue;
        };
        let weight = mapping.weight.abs();
        weighted_sum += sample * weight;
        total_weight += weight;
    }

    if total_weight <= 0.0 {
        return 0.0;
    }
    clamp01(weighted_sum / total_weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_constructor_sanitizes_bounds() {
        let mapping = Mapping::new(
            "/avatar/parameters/X",
            1.0,
            0.2,
            false,
            Curve::Linear,
            2.0,
            2.0,
        )
        .expect("mapping should be valid");
        assert_eq!(mapping.min, 2.0);
        assert_eq!(mapping.max, 3.0);
    }

    #[test]
    fn map_input_applies_deadzone_and_curve() {
        let mapping = Mapping::new(
            "/avatar/parameters/X",
            1.0,
            0.2,
            false,
            Curve::EaseOutQuad,
            0.0,
            1.0,
        )
        .expect("mapping should be valid");
        assert_eq!(map_input_value(0.1, &mapping), 0.0);

        let mapped = map_input_value(0.5, &mapping);
        assert!((mapped - 0.75).abs() < 1e-9);
    }

    #[test]
    fn map_input_applies_invert() {
        let mapping = Mapping::new(
            "/avatar/parameters/Y",
            1.0,
            0.0,
            true,
            Curve::Linear,
            0.0,
            1.0,
        )
        .expect("mapping should be valid");
        let mapped = map_input_value(0.3, &mapping);
        assert!((mapped - 0.7).abs() < 1e-9);
    }

    #[test]
    fn composite_uses_absolute_weights() {
        let mappings = vec![
            Mapping::new(
                "/avatar/parameters/A",
                1.0,
                0.0,
                false,
                Curve::Linear,
                0.0,
                1.0,
            )
            .expect("mapping should be valid"),
            Mapping::new(
                "/avatar/parameters/B",
                -3.0,
                0.0,
                false,
                Curve::Linear,
                0.0,
                1.0,
            )
            .expect("mapping should be valid"),
        ];

        let mut inputs = HashMap::new();
        inputs.insert("/avatar/parameters/A".to_string(), 0.2);
        inputs.insert("/avatar/parameters/B".to_string(), 0.8);

        let value = calculate_composite_intensity(&inputs, &mappings);
        assert!((value - 0.65).abs() < 1e-9);
    }
}
