use askama_axum::Template;
use axum::{
    error_handling::HandleErrorLayer,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use tokio::{
    net::{lookup_host, TcpListener},
    sync::{broadcast, Semaphore},
    time,
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower::{BoxError, ServiceBuilder};
use tower_http::{services::ServeDir, trace::TraceLayer};
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

const PING_TIMEOUT: Duration = Duration::from_millis(500);
const DNS_TIMEOUT: Duration = Duration::from_millis(500);
const EVENT_LOOP_MAJOR_CYCLE: Duration = Duration::from_secs(2);
const EVENT_LOOP_MINOR_CYCLE: Duration = Duration::from_millis(1);
const SSE_PERIOD: Duration = Duration::from_millis(1);

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

    let assets_path: std::path::PathBuf = std::env::var("APP_ASSETS_PATH")
        .unwrap_or_else(|_| "./assets".to_string())
        .into();
    let config_path: std::path::PathBuf = std::env::var("APP_CONFIG_FILE")
        .unwrap_or_else(|_| "devices.yml".to_string())
        .into();
    let bind_ip: IpAddr = std::env::var("APP_BIND_IP")
        .unwrap_or_else(|_| "0.0.0.0".to_string())
        .parse()?;
    let bind_port: u16 = std::env::var("APP_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()?;

    let config_file = std::fs::File::open(config_path)?;
    let devices: HashMap<String, Device> = serde_yaml::from_reader(config_file)?;

    let (events, _) = broadcast::channel(10);
    let state = Arc::new(AppState {
        devices,
        events,
        single_event_loop: Arc::new(Semaphore::new(1)),
    });

    let app = Router::new()
        .route("/", get(get_root))
        .route("/status-stream", get(get_status_stream))
        .route("/wake/:device_name", post(post_wake))
        .route("/api/devices", get(get_devices))
        .route(
            "/api/device/:device_name",
            get(get_device).post(post_device),
        )
        .nest_service("/assets", ServeDir::new(assets_path))
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

    let bind_addr = SocketAddr::from((bind_ip, bind_port));
    let listener = TcpListener::bind(bind_addr).await?;
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

type SharedState = Arc<AppState>;
type Devices = HashMap<String, Device>;

struct AppState {
    devices: Devices,
    events: broadcast::Sender<Event>,

    // Limits the number of event loop instances to zero or one
    //
    // The event loop is started when the SSE stream request is made and no event loop is currently
    // running.  Successive SSE stream requests will use the single event loop.  The event loop
    // terminates when all SSE streams are closed.
    single_event_loop: Arc<Semaphore>,
}

async fn get_device(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<Json<DeviceStatus>, StatusCode> {
    let devices = &state.devices;

    if devices.get(&device_name).is_some() {
        let status = ping_hostname(&device_name).await;
        let device_status = DeviceStatus {
            name: device_name.clone(),
            status,
        };
        Ok(Json(device_status))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn event_loop(state: SharedState) {
    let mut outer_interval = time::interval(EVENT_LOOP_MAJOR_CYCLE);
    let mut inner_interval = time::interval(EVENT_LOOP_MINOR_CYCLE);
    loop {
        for device_name in state.devices.keys() {
            let status = ping_hostname(device_name).await;
            let device_status = DeviceStatus {
                name: device_name.clone(),
                status,
            };
            let component = DeviceStatusComponent {
                device: device_status,
            };
            let data = component.to_string();
            let event = Event::default().data(data).event(device_name);
            let send_result = state.events.send(event);
            if send_result.is_err() {
                return;
            }
            inner_interval.tick().await;
        }
        outer_interval.tick().await;
    }
}

async fn resolve_hostname(device_name: &str) -> Result<IpAddr, ()> {
    let host = (device_name, 0);
    let timeout_result = time::timeout(DNS_TIMEOUT, lookup_host(host)).await;

    let lookup_result = timeout_result.map_err(|_| ())?;
    let mut addrs = lookup_result.map_err(|_| ())?;
    let addr = addrs.next().ok_or(())?;
    Ok(addr.ip())
}

async fn post_device(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<(), StatusCode> {
    let devices = &state.devices;

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

async fn ping_hostname(hostname: &str) -> PingStatus {
    let resolve_result = resolve_hostname(hostname).await;
    let ip = match resolve_result {
        Ok(ip) => ip,
        Err(_) => return PingStatus::DnsError,
    };

    let data = [8; 8];
    let ping_result = ping(&ip, PING_TIMEOUT, Arc::new(&data), None).await;
    match ping_result {
        Ok(_) => PingStatus::Online,
        Err(_) => PingStatus::Offline,
    }
}

async fn get_status_stream(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = state.events.subscribe();

    if let Ok(permit) = state.single_event_loop.clone().try_acquire_owned() {
        let event_loop_state = state.clone();
        tokio::spawn(async move {
            event_loop(event_loop_state).await;
            permit.forget();
        });
    }

    let stream = BroadcastStream::new(events)
        .filter_map(Result::ok)
        .map(Ok)
        .throttle(SSE_PERIOD);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn post_wake(
    Path(device_name): Path<String>,
    State(state): State<SharedState>,
) -> Result<(), StatusCode> {
    let devices = &state.devices;

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
    let devices = &state.devices;

    Json(devices.clone())
}

async fn get_root(State(state): State<SharedState>) -> RootPage {
    let devices = &state.devices;
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
