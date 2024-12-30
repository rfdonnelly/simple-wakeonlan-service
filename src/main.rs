use askama_axum::Template;
use axum::{
    error_handling::HandleErrorLayer,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::{self, Stream};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use tokio::{
    net::{lookup_host, TcpListener},
    sync::RwLock,
};
use tokio_stream::StreamExt;
use tower::{BoxError, ServiceBuilder};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use macaddr::MacAddr6 as MacAddr;
use ping_rs::send_ping_async as ping;
use wake_on_lan::MagicPacket;

use std::{
    collections::HashMap,
    convert::Infallible,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

#[derive(Template)]
#[template(path = "pages/root.html")]
struct RootPage {
    devices: Vec<DeviceStatus>,
}

#[derive(Template)]
#[template(path = "components/device-status.html")]
struct DeviceStatusComponent {
    device: DeviceStatus,
}

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

    let state = Arc::new(RwLock::new(AppState { devices }));

    let app = Router::new()
        .route("/", get(get_root))
        .route("/status/:device_name", get(get_status))
        .route("/status-sse", get(get_status_sse))
        .route("/wake/:device_name", post(post_wake))
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

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let listener = TcpListener::bind(addr).await?;
    tracing::debug!("listening on http://{}", listener.local_addr()?);

    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Device {
    #[serde(deserialize_with = "deserialize_from_str")]
    mac: MacAddr,
}

#[derive(Debug, Default, Clone, Serialize)]
struct DeviceStatus {
    name: String,
    // #[serde(serialize_with = "serialize_to_string")]
    // mac: MacAddr,
    status: PingStatus,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
enum PingStatus {
    Online,
    #[default]
    Offline,
    DnsError,
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

fn serialize_to_string<T, S>(v: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: ToString,
    S: Serializer,
{
    serializer.serialize_str(&v.to_string())
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
) -> Result<Json<DeviceStatus>, StatusCode> {
    let devices = &state.read().await.devices;

    if let Some(_) = devices.get(&device_name) {
        let status = ping_hostname(&device_name).await;
        let device_status = DeviceStatus {
            name: device_name.clone(),
            status: status,
        };
        Ok(Json(device_status))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn resolve_hostname(device_name: &str) -> Result<IpAddr, ()> {
    Ok(lookup_host((device_name, 0))
        .await
        .map_err(|_| ())?
        .next()
        .unwrap()
        .ip())
}

async fn post_device(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<(), StatusCode> {
    let devices = &state.read().await.devices;

    if let Some(device) = devices.get(&device_name) {
        let packet = MagicPacket::new(device.mac.as_bytes().try_into().unwrap());
        let ip_addr = resolve_hostname(&device_name)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let to_socket_addr = (ip_addr, 9);
        let from_socket_addr = (IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
        tracing::debug!("sending wol packet to {}", device_name);
        packet
            .send_to(to_socket_addr, from_socket_addr)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_status(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<DeviceStatusComponent, StatusCode> {
    let devices = &state.read().await.devices;

    if let Some(_) = devices.get(&device_name) {
        let status = ping_hostname(&device_name).await;
        let device_status = DeviceStatus {
            name: device_name.clone(),
            status: status,
        };
        Ok(DeviceStatusComponent { device: device_status })
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn ping_hostname(hostname: &str) -> PingStatus {
    let resolve_result = resolve_hostname(hostname).await;
    let ip = match resolve_result {
        Ok(ip) => ip,
        Err(_) => return PingStatus::DnsError,
    };

    let data = [8; 8];
    let ping_result = ping(&ip, Duration::from_secs(1), Arc::new(&data), None).await;
    match ping_result {
        Ok(_) => PingStatus::Online,
        Err(_) => PingStatus::Offline,
    }
}

async fn get_status_sse(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let device_names: Vec<_> = {
        let devices = &state.read().await.devices;
        devices.keys().cloned().collect()
    };

    let stream = stream::unfold(
        device_names.into_iter().cycle(),
        move |mut device_names_iter| {
            let device_name = device_names_iter.next().unwrap();
            async move {
                let status = ping_hostname(&device_name).await;
                let device_status = DeviceStatus {
                    name: device_name.clone(),
                    status: status,
                };
                let component = DeviceStatusComponent {
                    device: device_status,
                };
                let data = component.to_string();
                let event = Event::default().data(data).event(device_name);
                Some((event, device_names_iter))
            }
        },
    )
    .map(Ok)
    .throttle(Duration::from_secs(1));

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn post_wake(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<(), StatusCode> {
    let devices = &state.read().await.devices;

    if let Some(device) = devices.get(&device_name) {
        let packet = MagicPacket::new(device.mac.as_bytes().try_into().unwrap());
        let ip_addr = resolve_hostname(&device_name)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let to_socket_addr = (ip_addr, 9);
        let from_socket_addr = (IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
        tracing::debug!("sending wol packet to {}", device_name);
        packet
            .send_to(to_socket_addr, from_socket_addr)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn get_devices(State(state): State<SharedState>) -> Json<Devices> {
    let devices = &state.read().await.devices;

    Json(devices.clone())
}

async fn get_root(State(state): State<SharedState>) -> RootPage {
    let devices = &state.read().await.devices;
    let mut devices: Vec<_> = devices
        .keys()
        .map(|name| DeviceStatus {
            name: name.clone(),
            status: PingStatus::Offline,
        })
        .collect();
    devices.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    RootPage { devices }
}
