//! Per-plugin TOML configuration (`<plugin-stem>.toml` next to the
//! `.wasm`).
//!
//! Schema mirrors `docs/decisions/0004-wasm-plugins.md`. Sections
//! that aren't enforced yet (priority, capabilities, CPU budget) are
//! still parsed so operators can author forward-compatible configs
//! today and have the host honor them as the runtime catches up.
//!
//! Missing file → [`PluginConfig::default()`]. Missing section →
//! per-section default. We deliberately accept unknown keys (no
//! `deny_unknown_fields`) to avoid breaking operator upgrades when
//! the schema grows.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level config. All sections optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct PluginConfig {
    /// `[plugin]` — identity + enable flag.
    pub plugin: PluginSection,
    /// `[limits]` — resource caps. Only `max-memory-mb` is wired
    /// in v0.
    pub limits: LimitsSection,
    /// `[capabilities]` — host-permission grants + the JSON blob
    /// passed to `init()`. Only `plugin-config` is consumed in v0;
    /// `filesystem` / `network` are accepted for forward compat
    /// but not yet honored (the WasiCtx is deny-everything either
    /// way).
    pub capabilities: CapabilitiesSection,
}

/// `[plugin]` TOML section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct PluginSection {
    /// Operator-friendly name. When set, must match the wasm file
    /// stem; mismatch logs a warning.
    pub name: Option<String>,
    /// Skip loading this plugin entirely.
    pub enabled: bool,
    /// Future: lower runs first. Parsed but inert in v0 (we have no
    /// ordered fan-out yet).
    pub priority: i32,
}

impl Default for PluginSection {
    fn default() -> Self {
        Self {
            name: None,
            enabled: true,
            priority: 100,
        }
    }
}

/// `[limits]` TOML section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LimitsSection {
    /// wasmtime per-instance linear memory cap. Default 32 MB
    /// matches the design doc.
    pub max_memory_mb: u32,
    /// Per-call CPU budget (epoch interrupt). Parsed but inert in
    /// v0 — the epoch ticker isn't wired yet.
    pub max_cpu_ms_per_msg: u32,
    /// RSS leak threshold for instance recycling. Parsed but inert
    /// in v0.
    pub max_rss_leak_mb: u32,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            max_memory_mb: 32,
            max_cpu_ms_per_msg: 1,
            max_rss_leak_mb: 0,
        }
    }
}

/// `[capabilities]` TOML section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CapabilitiesSection {
    /// Future: list of preopen paths. Inert in v0.
    pub filesystem: Vec<String>,
    /// Future: allowlist of host:port pairs. Inert in v0.
    pub network: Vec<String>,
    /// JSON blob passed to the plugin's `init()` call. Empty
    /// string defaults to `"{}"` so plugins always see valid JSON.
    pub plugin_config: String,
}

impl PluginConfig {
    /// JSON to hand to the plugin's `init()`. Empty string in
    /// `plugin-config` becomes `"{}"` so plugins can blindly
    /// `serde_json::from_str` without an empty-input guard.
    #[must_use]
    pub fn init_json(&self) -> &str {
        if self.capabilities.plugin_config.is_empty() {
            "{}"
        } else {
            &self.capabilities.plugin_config
        }
    }

    /// Memory cap as bytes for `wasmtime::ResourceLimiter`. Caps at
    /// `usize::MAX` on tiny targets (matters for cross-compiles, not
    /// any host we ship).
    #[must_use]
    pub fn memory_bytes_cap(&self) -> usize {
        usize::try_from(self.limits.max_memory_mb).unwrap_or(usize::MAX) * 1024 * 1024
    }

    /// Try to load `<dir>/<stem>.toml`. Missing file is **not** an
    /// error — returns `Ok(None)` so the caller can fall back to
    /// `PluginConfig::default()`. Returns `Err` only on read /
    /// parse failure.
    ///
    /// # Errors
    /// - The file exists but couldn't be read.
    /// - The TOML is syntactically invalid.
    /// - A field has the wrong type.
    pub fn try_load(dir: &Path, stem: &str) -> Result<Option<(Self, PathBuf)>, ConfigError> {
        let path = dir.join(format!("{stem}.toml"));
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| ConfigError::Read {
            path: path.clone(),
            source: e,
        })?;
        let cfg: Self = toml::from_str(&raw).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            source: Box::new(e),
        })?;
        Ok(Some((cfg, path)))
    }
}

/// Errors raised by [`PluginConfig::try_load`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// I/O error reading the config file.
    #[error("read {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// TOML parse / type error.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying TOML decode error.
        #[source]
        source: Box<toml::de::Error>,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled_with_empty_init_json() {
        let cfg = PluginConfig::default();
        assert!(cfg.plugin.enabled);
        assert_eq!(cfg.init_json(), "{}");
        assert_eq!(cfg.memory_bytes_cap(), 32 * 1024 * 1024);
    }

    #[test]
    fn full_config_round_trips() {
        let raw = r#"
            [plugin]
            name = "geofence-redact"
            enabled = true
            priority = 50

            [limits]
            max-memory-mb = 64
            max-cpu-ms-per-msg = 5
            max-rss-leak-mb = 8

            [capabilities]
            filesystem = ["/var/tak/state"]
            network = ["127.0.0.1:9000"]
            plugin-config = '{ "drop_below_lat": 30.0 }'
        "#;
        let cfg: PluginConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.plugin.name.as_deref(), Some("geofence-redact"));
        assert!(cfg.plugin.enabled);
        assert_eq!(cfg.plugin.priority, 50);
        assert_eq!(cfg.limits.max_memory_mb, 64);
        assert_eq!(cfg.limits.max_cpu_ms_per_msg, 5);
        assert_eq!(cfg.capabilities.filesystem.len(), 1);
        assert_eq!(cfg.capabilities.network.len(), 1);
        assert_eq!(cfg.init_json(), r#"{ "drop_below_lat": 30.0 }"#);
        assert_eq!(cfg.memory_bytes_cap(), 64 * 1024 * 1024);
    }

    #[test]
    fn empty_sections_get_defaults() {
        let cfg: PluginConfig = toml::from_str("").unwrap();
        assert!(cfg.plugin.enabled);
        assert_eq!(cfg.limits.max_memory_mb, 32);
    }

    #[test]
    fn disabled_flag_round_trips() {
        let cfg: PluginConfig = toml::from_str("[plugin]\nenabled = false\n").unwrap();
        assert!(!cfg.plugin.enabled);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        // Forward compat: a future schema field shouldn't break
        // older hosts during a rolling upgrade.
        let cfg: PluginConfig =
            toml::from_str("[plugin]\nfuture-field = 42\n").expect("unknown keys ignored");
        assert!(cfg.plugin.enabled);
    }
}
