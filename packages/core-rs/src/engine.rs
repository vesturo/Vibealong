use std::collections::HashMap;

use crate::mapping::{calculate_composite_intensity, map_input_value, Mapping};
use crate::osc::{extract_numeric_arg, OscMessage};

#[derive(Debug, Clone, PartialEq)]
pub struct SourceMeta {
    pub address: String,
    pub arg_type: String,
}

#[derive(Debug, Default)]
pub struct BridgeEngine {
    pub mappings: Vec<Mapping>,
    pub current_inputs: HashMap<String, f64>,
    pub target_intensity: f64,
    pub last_source: SourceMeta,
}

impl Default for SourceMeta {
    fn default() -> Self {
        Self {
            address: String::new(),
            arg_type: "f".to_string(),
        }
    }
}

impl BridgeEngine {
    pub fn new(mappings: Vec<Mapping>) -> Self {
        Self {
            mappings,
            ..Self::default()
        }
    }

    pub fn process_messages(&mut self, messages: &[OscMessage]) -> bool {
        let mut touched = false;
        for message in messages {
            for mapping in &self.mappings {
                if mapping.address != message.address {
                    continue;
                }
                let Some(raw_value) = extract_numeric_arg(&message.args) else {
                    continue;
                };
                let mapped = map_input_value(raw_value, mapping);
                self.current_inputs.insert(mapping.address.clone(), mapped);
                self.last_source.address = message.address.clone();
                self.last_source.arg_type = message
                    .arg_type
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "f".to_string());
                touched = true;
            }
        }
        if touched {
            self.target_intensity =
                calculate_composite_intensity(&self.current_inputs, &self.mappings);
        }
        touched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::Curve;
    use crate::osc::{OscArg, OscMessage};

    #[test]
    fn engine_updates_target_from_mapped_message() {
        let mapping = Mapping::new(
            "/avatar/parameters/SPS_Contact",
            1.0,
            0.0,
            false,
            Curve::Linear,
            0.0,
            1.0,
        )
        .expect("mapping should be valid");
        let mut engine = BridgeEngine::new(vec![mapping]);
        let messages = vec![OscMessage {
            address: "/avatar/parameters/SPS_Contact".to_string(),
            args: vec![OscArg::Float(0.4)],
            arg_type: Some('f'),
            arg_types: "f".to_string(),
        }];
        let touched = engine.process_messages(&messages);
        assert!(touched);
        assert!((engine.target_intensity - 0.4).abs() < 1e-6);
        assert_eq!(engine.last_source.address, "/avatar/parameters/SPS_Contact");
        assert_eq!(engine.last_source.arg_type, "f");
    }
}
