use axum::{
    error_handling::HandleErrorLayer,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};

use serde::{de, Deserialize, Deserializer, Serialize};

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use std::fmt::Display;
use std::str::FromStr;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::{BoxError, ServiceBuilder};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use macaddr::MacAddr6 as MacAddr;
use wake_on_lan::MagicPacket as WolPacket;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=debug,tower_http=debug", env!("CARGO_CRATE_NAME")).into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let devices_file = std::fs::File::open("devices.yml")?;
    let devices: HashMap<String, Device> = serde_yaml::from_reader(devices_file)?;
    tracing::info!("loaded devices {:?}", devices);

    let state = Arc::new(RwLock::new(AppState { devices }));

    let app = Router::new()
        .route("/devices", get(get_devices))
        .route("/device/:device_name", get(get_device).post(post_device))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|error: BoxError| async move {
                    if error.is::<tower::timeout::error::Elapsed>() {
                        Ok(StatusCode::REQUEST_TIMEOUT)
                    } else {
                        Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Unhandled internal error: {error}"),
                        ))
                    }
                }))
                .timeout(Duration::from_secs(10))
                .layer(TraceLayer::new_for_http())
                .into_inner(),
        )
        .with_state(Arc::clone(&state));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = TcpListener::bind(addr).await?;
    tracing::debug!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app.into_make_service()).await?;


    Ok(())
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct Device {
    #[serde(deserialize_with = "deserialize_from_str")]
    mac: MacAddr,
}

fn deserialize_from_str<'de, S, D>(deserializer: D) -> Result<S, D::Error>
where
    S: FromStr,
        S::Err: Display,
            D: Deserializer<'de>,
{
        let s: String = Deserialize::deserialize(deserializer)?;
            S::from_str(&s).map_err(de::Error::custom)
}

type SharedState = Arc<RwLock<AppState>>;
type Devices = HashMap<String, Device>;

#[derive(Default)]
struct AppState {
    devices: Devices,
}

async fn get_device(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<Json<Device>, StatusCode> {
    let devices = &state.read().unwrap().devices;

    if let Some(device) = devices.get(&device_name) {
        Ok(Json(device.clone()))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn post_device(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<(), StatusCode> {
    let devices = &state.read().unwrap().devices;

    if let Some(device) = devices.get(&device_name) {
        let packet = WolPacket::new(device.mac.as_bytes().try_into().unwrap());
        tracing::debug!("sending wol packet to {}", device_name);
        packet.send().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_devices(State(state): State<SharedState>) -> Json<Devices> {
    let devices = &state.read().unwrap().devices;

    Json(devices.clone())
}
