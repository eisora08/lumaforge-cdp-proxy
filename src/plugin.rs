use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    #[serde(default, alias = "schemaVersion")]
    pub schema_version: Option<u32>,
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(default = "default_version")]
    pub version: String,
    pub description: Option<String>,
    pub author: Option<String>,
    #[serde(default)]
    pub has_detect: bool,
    pub activation: Option<ActivationConfig>,
    pub backend: Option<BackendConfig>,
}

fn default_version() -> String {
    "0.0.0".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationConfig {
    #[serde(default, alias = "cefInjection")]
    pub cef_injection: bool,
    #[serde(default = "default_inject_script", alias = "injectScript")]
    pub inject_script: String,
    #[serde(default, alias = "targetUrl")]
    pub target_url: Option<String>,
}

fn default_inject_script() -> String {
    "inject.js".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(default = "default_backend_type")]
    pub backend_type: String,
    #[serde(default = "default_backend_script")]
    pub script: String,
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_backend_type() -> String {
    "lua".to_string()
}

fn default_backend_script() -> String {
    "backend.lua".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub source: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub _id: String,
    pub name: String,
    pub _version: String,
    pub code: String,
    pub target_url: Option<String>,
    pub _dir: PathBuf,
    pub backend_config: Option<BackendConfig>,
}
