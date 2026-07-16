#![no_std]
#![recursion_limit = "1024"]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
};
use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer};
use esp_println::println;
use esp_radio::wifi::{
    sta::StationConfig, Config, ControllerConfig, PowerSaveMode, WifiController, Interface,
};
use picoserve::{
    extract::State,
    response::{IntoResponse, Response},
    routing::get,
    AppRouter, AppWithStateBuilder,
};

pub mod mdns;

// https://github.com/embassy-rs/static-cell/issues/16
#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init_with(|| $val)
    }};
}

pub const SSID: &str = env!("SSID");
pub const PASSWORD: &str = env!("PASSWORD");

// TODO: Remove this horrible workaround once https://github.com/tkaitchuck/constrandom/issues/36 has been resolved
const UUID_SEED: [u8; 16] = [
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
];

/// Produce an urn that can be used as id
#[must_use]
pub fn get_urn_or_uuid(stack: Stack) -> String {
    if cfg!(feature = "uuid-id") {
        let uuid = uuid::Builder::from_random_bytes(UUID_SEED).into_uuid();

        uuid.urn().to_string()
    } else {
        let device_id = stack.hardware_address().to_string();
        format!("urn:example/shtc3/{device_id}")
    }
}

/// # Panics
#[must_use]
pub fn to_json_response<T: serde::Serialize>(data: &T) -> impl IntoResponse {
    let body = serde_json::to_string(data).unwrap();
    Response::ok(body).with_header("Content-Type", "application/json")
}

#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    loop {
        if controller.is_connected() {
            // wait until we're no longer connected
            controller.wait_for_disconnect_async().await.ok();
            Timer::after(Duration::from_millis(5000)).await;
        }

        println!("About to connect...");
        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await;
}

#[allow(clippy::similar_names)]
pub async fn web_task<Props: AppWithStateBuilder>(
    task_id: usize,
    stack: Stack<'static>,
    app: &'static AppRouter<Props>,
    config: &'static picoserve::Config,
    state: &'static Props::State,
) {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(&app.shared().with_state(state), config, &mut http_buffer)
        .listen_and_serve(task_id, stack, port, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await;
}

/// A trait for application states that carry a serialized Thing Description.
pub trait TdState {
    /// The serialized Thing Description (JSON), served at `/`.
    fn td(&self) -> &'static str;
}

/// Build the initial router with the standard WoT routes: the Thing Description
/// at `/` (and `/` via `/.well-known/wot` redirect).
///
/// Call this instead of `picoserve::Router::new()` at the start of `build_app`.
pub fn td_routes<S: TdState + Clone + Copy>() -> picoserve::Router<
    impl picoserve::routing::PathRouter<S>,
    S,
> {
    picoserve::Router::new()
        .route(
            "/",
            get(|State(state): State<S>| async move {
                picoserve::response::Response::ok(state.td())
                    .with_header("Content-Type", "application/td+json")
            }),
        )
        .route(
            "/.well-known/wot",
            get(|| async { picoserve::response::Redirect::to("/") }),
        )
}

///
/// Polls the watch with a 15s timeout, emitting `value_changed` events (or a
/// keepalive on timeout). Generic over the value type `T`.
pub struct SseEvents<'a, T: Clone + Send + 'static>(
    pub embassy_sync::watch::Receiver<'a, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, T, 2>,
);

impl<T> picoserve::response::sse::EventSource for SseEvents<'_, T>
where
    T: Clone + Send + core::fmt::Display + 'static,
{
    async fn write_events<W: picoserve::io::Write>(
        mut self,
        mut writer: picoserve::response::sse::EventWriter<'_, W>,
    ) -> Result<(), W::Error> {
        loop {
            match embassy_time::with_timeout(
                embassy_time::Duration::from_secs(15),
                self.0.changed(),
            )
            .await
            {
                Ok(value) => {
                    writer
                        .write_event("value_changed", alloc::format!("{value}").as_str())
                        .await?;
                }
                Err(_) => writer.write_keepalive().await?,
            }
        }
    }
}


/// Peripherals consumed by the networking stack during [`EspThing::run`].
///
/// Demos extract these from `Peripherals` in [`EspThingState::new`] and return
/// them so the library can bring up Wi-Fi / embassy-net.
pub struct NetworkPeripherals<'d> {
    pub timg0: esp_hal::peripherals::TIMG0<'d>,
    pub sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'d>,
    pub wifi: esp_hal::peripherals::WIFI<'d>,
}

pub trait EspThingState {
    /// Consume the full `Peripherals`, extract hardware for the thing, and return
    /// the state alongside the peripherals the networking stack needs.
    ///
    /// The serialized TD is set later via [`Self::set_td`] once the network is up.
    fn new(
        spawner: embassy_executor::Spawner,
        peripherals: esp_hal::peripherals::Peripherals,
    ) -> (&'static Self, NetworkPeripherals<'static>);

    /// Set the serialized Thing Description, called after the network is up.
    fn set_td(&self, td: &'static str);
}

pub trait EspThing<Props>
where
    Props: AppWithStateBuilder + Default + 'static,
    Props::State: EspThingState + 'static,
{
    const NAME: &'static str;

    fn build_td(name: &str, base_uri: String, id: String) -> wot_td::Thing;

    #[allow(async_fn_in_trait, clippy::must_use_candidate)]
    async fn run(spawner: embassy_executor::Spawner) {
        esp_println::logger::init_logger_from_env();
        let peripherals = esp_hal::init(
            esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()),
        );

        esp_alloc::heap_allocator!(size: 200 * 1024);

        // Let the demo extract its hardware and hand back the network peripherals.
        let (app_state, net_peripherals) = Props::State::new(spawner, peripherals);

        let timg0 = esp_hal::timer::timg::TimerGroup::new(net_peripherals.timg0);
        let sw_int = esp_hal::interrupt::software::SoftwareInterruptControl::new(
            net_peripherals.sw_interrupt,
        );
        esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

        let (mut controller, interfaces) =
            esp_radio::wifi::new(net_peripherals.wifi, ControllerConfig::default()).unwrap();

        // PowerSaveMode::Maximum breaks WiFi on the ESP32-C6 (known issue,
        // esp-rs/esp-hal#3014, #3075, #3079). Use None for reliable connectivity.
        controller.set_power_saving(PowerSaveMode::None).unwrap();

        let station_config = Config::Station(
            StationConfig::default()
                .with_ssid(SSID)
                .with_password(PASSWORD.into()),
        );
        controller.set_config(&station_config).unwrap();

        let wifi_interface = interfaces.station;

        let config = embassy_net::Config::dhcpv4(Default::default());

        let rng = esp_hal::rng::Rng::new();
        let seed = (rng.random() as u64) << 32 | rng.random() as u64;

        let mac_address = wifi_interface.mac_address();
        println!("Device MAC address: {mac_address:02x?}");

        // Init network stack
        let (stack, runner) = embassy_net::new(
            wifi_interface,
            config,
            mk_static!(embassy_net::StackResources<{ 8 * mdns::MDNS_STACK_SIZE + 2 }>, embassy_net::StackResources::new()),
            seed,
        );

        spawner.spawn(connection(controller).expect("connection"));
        spawner.spawn(net_task(runner).expect("net_task"));

        loop {
            if stack.is_link_up() {
                break;
            }
            Timer::after(Duration::from_millis(500)).await;
        }

        let base_uri;
        println!("Waiting to get IP address...");
        loop {
            if let Some(config) = stack.config_v4() {
                println!("Got IP: {}", config.address);
                base_uri = format!("http://{}", config.address.address());
                break;
            }
            Timer::after(Duration::from_millis(500)).await;
        }

        let id = get_urn_or_uuid(stack);

        let name = Self::NAME;

        let td = Self::build_td(Self::NAME, base_uri, id);

        let td = serde_json::to_string(&td).unwrap();

        let td = mk_static!(String, td);
        Props::State::set_td(app_state, td.as_str());

        let app = alloc::boxed::Box::leak(alloc::boxed::Box::new(Props::default().build_app()));

        let config = mk_static!(
            picoserve::Config,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Duration::from_secs(5),
                persistent_start_read_request: Duration::from_secs(1),
                read_request: Duration::from_secs(1),
                write: Duration::from_secs(1),
            })
            .keep_connection_alive()
        );

        spawner.spawn(mdns::mdns_task(stack, rng, name).expect("mdns"));

        let web_tasks: [_; 4] = core::array::from_fn(|id| {
            alloc::boxed::Box::pin(<() as WebTask<Props>>::spawn(
                id, stack, app, config, app_state,
            ))
        });

        embassy_futures::join::join_array(web_tasks).await;
    }
}

trait WebTask<Props: picoserve::AppWithStateBuilder> {
    type Fut: core::future::Future<Output = ()> + 'static;

    fn spawn(
        id: usize,
        stack: Stack<'static>,
        app: &'static AppRouter<Props>,
        config: &'static picoserve::Config,
        state: &'static Props::State,
    ) -> Self::Fut;
}

impl<Props: picoserve::AppWithStateBuilder + 'static> WebTask<Props> for () {
    type Fut = impl core::future::Future<Output = ()> + 'static;

    fn spawn(
        id: usize,
        stack: Stack<'static>,
        app: &'static AppRouter<Props>,
        config: &'static picoserve::Config,
        state: &'static Props::State,
    ) -> Self::Fut {
        web_task::<Props>(id, stack, app, config, state)
    }
}
