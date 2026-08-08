//! Acting on the relay's own control topics.
//!
//! The message path recognises control topics in Rust ([`Core::control_kind`])
//! and hands them here. Everything that still needs Python — `global_config`,
//! `orjson`, and the two callbacks on the relay object — lives behind
//! [`ControlSink`], so the data path never has to know a Python object exists.
//!
//! There is one implementor today, [`PyControlSink`]. The seam is here so the
//! native one can take its place without touching [`crate::mqtt::ingress_worker`].
//!
//! [`Core::control_kind`]: crate::process::Core::control_kind

use std::sync::Arc;

use log::{error, info};
use pyo3::intern;
use pyo3::prelude::*;

use crate::mqtt::MqttShared;
use crate::process::{self, ControlTopic};
use crate::util::loggable;

/// Somewhere a recognised control topic can be acted on.
pub(crate) trait ControlSink: Send + Sync {
    /// Act on one control topic.
    ///
    /// Reports failure as text rather than as an error type: the caller only
    /// logs it, next to the topic the message arrived on, and implementors do
    /// not share an error type.
    fn dispatch(&self, kind: ControlTopic, payload: &[u8]) -> Result<(), String>;
}

/// The control topics as they behave while `main.py` is still in charge.
///
/// Holds exactly what has to be reached across the language boundary. Note it
/// does *not* hold the [`Core`](crate::process::Core): the only thing it wanted
/// from it was the response topic, which is copied in once at construction.
pub(crate) struct PyControlSink {
    global_config: Py<PyAny>,
    relay_main_obj: Py<PyAny>,
    /// Shared with the Python-facing `MqttClient`, so a config response
    /// publishes straight from Rust instead of calling back into Python.
    mqtt_shared: Arc<MqttShared>,
    orjson_obj: Py<PyAny>,
    config_response: String,
}

impl PyControlSink {
    pub(crate) fn new(
        global_config: Py<PyAny>,
        relay_main_obj: Py<PyAny>,
        mqtt_shared: Arc<MqttShared>,
        orjson_obj: Py<PyAny>,
        config_response: String,
    ) -> Self {
        PyControlSink {
            global_config,
            relay_main_obj,
            mqtt_shared,
            orjson_obj,
            config_response,
        }
    }

    /// The dispatch itself, with the GIL already held.
    ///
    /// Kept separate from the trait method so `MiniserverDataProcessor::handle_control`
    /// can propagate a `PyErr` to its Python caller instead of a string.
    pub(crate) fn dispatch_py(
        &self,
        py: Python<'_>,
        kind: ControlTopic,
        message: &[u8],
    ) -> PyResult<()> {
        match kind {
            ControlTopic::MiniserverStartup => {
                let sync_on = self
                    .global_config
                    .bind(py)
                    .getattr(intern!(py, "miniserver"))?
                    .getattr(intern!(py, "sync_with_miniserver"))?
                    .extract::<bool>()?;
                if sync_on {
                    info!("Miniserver startup detected, resyncing whitelist");
                    self.relay_main_obj
                        .bind(py)
                        .call_method0(intern!(py, "schedule_miniserver_sync"))?;
                }
            }
            ControlTopic::ConfigGet => {
                // global_config.get_safe_config -> orjson.dumps -> publish.
                let safe_cfg = self
                    .global_config
                    .bind(py)
                    .call_method0(intern!(py, "get_safe_config"))?;
                let serialized = self
                    .orjson_obj
                    .bind(py)
                    .call_method1(intern!(py, "dumps"), (safe_cfg,))?;
                self.mqtt_shared.publish_detached(
                    self.config_response.clone(),
                    serialized.extract::<Vec<u8>>()?,
                );
            }
            ControlTopic::ConfigSet | ControlTopic::ConfigAdd | ControlTopic::ConfigRemove => {
                let mode = kind.update_mode().expect("set/add/remove carry a mode");
                let text = process::decode_payload("config", message);
                match self
                    .orjson_obj
                    .bind(py)
                    .call_method1(intern!(py, "loads"), (&*text,))
                {
                    Ok(py_obj) => {
                        let updated = self
                            .global_config
                            .bind(py)
                            .call_method1(intern!(py, "update_fields"), (py_obj, mode));
                        if let Err(e) = updated {
                            error!("Error updating configuration: {e:?}");
                        } else {
                            info!("Configuration updated via MQTT. Restarting program.");
                            let _ = self
                                .relay_main_obj
                                .bind(py)
                                .call_method0(intern!(py, "restart_relay"));
                        }
                    }
                    Err(e) => error!(
                        "Invalid JSON format in MQTT message '{}': {e:?}",
                        loggable(&text)
                    ),
                }
            }
            ControlTopic::ConfigReload => {
                info!("Reloading configuration. Restarting program.");
                let _ = self
                    .relay_main_obj
                    .bind(py)
                    .call_method0(intern!(py, "restart_relay"));
            }
        }
        Ok(())
    }
}

impl ControlSink for PyControlSink {
    fn dispatch(&self, kind: ControlTopic, payload: &[u8]) -> Result<(), String> {
        Python::attach(|py| {
            self.dispatch_py(py, kind, payload)
                .map_err(|e| e.to_string())
        })
    }
}
