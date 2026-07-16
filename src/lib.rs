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
    response::{IntoResponse, Response},
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

#[allow(non_snake_case)]
pub struct ThingPeripherals<'a> {
    pub I2C0: esp_hal::peripherals::I2C0<'a>,
    pub GPIO2: esp_hal::peripherals::GPIO2<'a>,
    pub GPIO8: esp_hal::peripherals::GPIO8<'a>,
    pub GPIO9: esp_hal::peripherals::GPIO9<'a>,
    pub GPIO10: esp_hal::peripherals::GPIO10<'a>,
    pub RMT: esp_hal::peripherals::RMT<'a>,
    pub TSENS: esp_hal::peripherals::TSENS<'a>,
}

pub trait EspThingState {
    fn new(
        spawner: embassy_executor::Spawner,
        td: String,
        thing_peripherals: ThingPeripherals<'static>,
    ) -> &'static Self;
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

        let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
        let sw_int =
            esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
        esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

        let (mut controller, interfaces) =
            esp_radio::wifi::new(peripherals.WIFI, ControllerConfig::default()).unwrap();

        // Power-save was previously set via ControllerConfig; in 0.18 it must be
        // applied explicitly, otherwise the radio runs at full power (hot + thirsty).
        controller.set_power_saving(PowerSaveMode::Maximum).unwrap();

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

        let thing_peripherals = ThingPeripherals {
            I2C0: peripherals.I2C0,
            GPIO2: peripherals.GPIO2,
            GPIO8: peripherals.GPIO8,
            GPIO9: peripherals.GPIO9,
            GPIO10: peripherals.GPIO10,
            RMT: peripherals.RMT,
            TSENS: peripherals.TSENS,
        };

        let app_state = Props::State::new(spawner, td, thing_peripherals);

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
