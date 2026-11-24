//! Helpers shared by tests that drive the HTTP API.

use super::{AppState, router};
use crate::archive::{Archive, ArchiveConfig};
use crate::devices::Devices;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use darm_core::ledger::ChainConfig;
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn state_for(dir: &std::path::Path, config: ArchiveConfig) -> AppState {
    let archive = Archive::open(config).unwrap();
    let devices = Devices::open(archive.ledger.chain_id(), dir).unwrap();
    AppState {
        archive: Arc::new(Mutex::new(archive)),
        write_token: Some(Arc::from(TOKEN)),
        read_token: None,
        devices: Arc::new(Mutex::new(devices)),
        enroll_token: None,
    }
}

/// An archive for the validator seeded `[5; 32]`, like the API tests.
pub fn app(dir: &std::path::Path) -> (Router, AppState) {
    let validator = darm_core::NodeIdentity::from_secret_bytes(&[5; 32]).node_id();
    let state = state_for(
        dir,
        ArchiveConfig {
            data_dir: Some(dir.to_path_buf()),
            chain_name: "archive-test".into(),
            validators: vec![validator],
            activations: Default::default(),
        },
    );
    (router(state.clone()), state)
}

/// An archive that follows `config` (same name, validators and schedule).
pub fn app_for_chain(dir: &std::path::Path, config: &ChainConfig) -> (Router, AppState) {
    let state = state_for(
        dir,
        ArchiveConfig {
            data_dir: Some(dir.to_path_buf()),
            chain_name: config.name.clone(),
            validators: config.validators.members().map(|(n, _)| *n).collect(),
            activations: config.activations.clone(),
        },
    );
    (router(state.clone()), state)
}

pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}
