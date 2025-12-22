use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub input_method_names: HashMap<String, String>,
    pub overlay: OverlayConfig,
    pub animation: AnimationConfig,
}

#[derive(Debug, Deserialize)]
pub struct OverlayConfig {
    pub width: u32,
    pub height: u32,
    pub font_size: f64,
}

#[derive(Debug, Deserialize)]
pub struct AnimationConfig {
    pub display_duration_ms: u64,
    pub fade_duration_ms: u64,
    pub fade_frames: u32,
}

impl Config {
    pub fn load() -> Self {
        const CONFIG_STR: &str = include_str!("../config.ron");
        ron::from_str(CONFIG_STR).expect("Failed to parse config.ron")
    }

    pub fn get_display_text(&self, input_method: &str) -> String {
        self.input_method_names
            .get(input_method)
            .cloned()
            .unwrap_or_else(|| input_method.to_string())
    }
}
