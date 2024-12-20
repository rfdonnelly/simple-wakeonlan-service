use axum::{
    error_handling::HandleErrorLayer,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};

use serde::Serialize;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::{BoxError, ServiceBuilder};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

    let devices = {
        let mut devices = HashMap::new();
        devices.insert(
            String::from("device0"),
            Device {
                pdu: String::from("pdu0"),
                outlet: 0,
                ..Default::default()
            },
        );
        devices.insert(
            String::from("device1"),
            Device {
                pdu: String::from("pdu1"),
                outlet: 0,
                ..Default::default()
            },
        );
        devices
    };
    let state = Arc::new(RwLock::new(AppState { devices }));

    let app = Router::new()
        .route("/devices", get(get_devices))
        .route("/device/:device_name", get(get_device))
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

    // write address like this to not make typos
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = TcpListener::bind(addr).await?;
    tracing::debug!("listening on {}", listener.local_addr().unwrap());

    axum::serve(listener, app.into_make_service()).await?;


    Ok(())
}

#[derive(Debug, Default, Clone, Serialize)]
struct Device {
    pdu: String,
    outlet: u32,
    user: Option<String>,
    origin: Option<String>,
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

    tracing::debug!("{:?}", &device_name);
    tracing::debug!("{:?}", &devices);

    if let Some(device) = devices.get(&device_name) {
        Ok(Json(device.clone()))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_devices(State(state): State<SharedState>) -> Json<Devices> {
    let devices = &state.read().unwrap().devices;

    Json(devices.clone())
}
