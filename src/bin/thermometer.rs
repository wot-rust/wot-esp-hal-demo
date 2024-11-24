#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
};
use const_random::const_random;
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, watch::Watch};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    i2c::master::{AnyI2c, Config, I2c},
    prelude::*,
    rng::Rng,
    timer::timg::TimerGroup,
    Blocking,
};
use esp_println::println;
use esp_wifi::{
    init,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiController,
};
use picoserve::{
    extract::State,
    response::{self, Response, StatusCode},
    routing::get,
};
use shtcx::{self, sensor_class::Sht2Gen, shtc3, PowerMode, ShtCx};
use uuid::Builder;
use wot_td::{builder::*, Thing};

#[derive(Clone, Copy)]
struct AppState {
    sensor: &'static Mutex<
        CriticalSectionRawMutex,
        &'static mut ShtCx<Sht2Gen, &'static mut I2c<'static, Blocking, AnyI2c>>,
    >,
    td: &'static str,
}

impl AppState {
    /// Returns the latest temperature measurement in degrees celsius.
    async fn get_temperature(&self) -> Result<f32, shtcx::Error<esp_hal::i2c::master::Error>> {
        Ok(self
            .sensor
            .lock()
            .await
            .get_temperature_measurement_result()?
            .as_degrees_celsius())
    }

    /// Returns the latest humidity measurement in percent.
    async fn get_humidity(&self) -> Result<f32, shtcx::Error<esp_hal::i2c::master::Error>> {
        Ok(self
            .sensor
            .lock()
            .await
            .get_humidity_measurement_result()?
            .as_percent())
    }
}

type AppRouter = impl picoserve::routing::PathRouter<AppState>;

const WEB_TASK_POOL_SIZE: usize = 1;

const UUID_SEED: [u8; 16] = const_random!([u8; 16]);

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    id: usize,
    stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
    app: &'static picoserve::Router<AppRouter, AppState>,
    config: &'static picoserve::Config<Duration>,
    state: &'static AppState,
) -> ! {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::listen_and_serve_with_state(
        id,
        app,
        config,
        stack,
        port,
        &mut tcp_rx_buffer,
        &mut tcp_tx_buffer,
        &mut http_buffer,
        state,
    )
    .await
}

#[embassy_executor::task]
async fn temperature_write_task(state: &'static AppState) -> ! {
    let sender = WATCH.sender();

    loop {
        Timer::after(Duration::from_secs(15)).await;

        let temperature = state.get_temperature().await;

        if let Ok(temperature) = temperature {
            sender.send(temperature);
        }
    }
}

// https://github.com/embassy-rs/static-cell/issues/16
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init_with(|| $val)
    }};
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

fn generate_uuid_urn() -> alloc::string::String {
    let uuid = Builder::from_random_bytes(UUID_SEED).into_uuid();

    uuid.urn().to_string()
}

static WATCH: Watch<CriticalSectionRawMutex, f32, 2> = Watch::new();

struct Events<'a>(embassy_sync::watch::Receiver<'a, CriticalSectionRawMutex, f32, 2>);

impl<'a> response::sse::EventSource for Events<'a> {
    async fn write_events<W: picoserve::io::Write>(
        mut self,
        mut writer: response::sse::EventWriter<W>,
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
                        .write_event("value_changed", value.to_string().as_str())
                        .await?
                }
                Err(_) => writer.write_keepalive().await?,
            }
        }
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init({
        let mut config = esp_hal::Config::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    esp_alloc::heap_allocator!(72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    let init = &*mk_static!(
        EspWifiController<'static>,
        init(
            timg0.timer0,
            Rng::new(peripherals.RNG),
            peripherals.RADIO_CLK,
        )
        .unwrap()
    );

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    use esp_hal::timer::systimer::{SystemTimer, Target};
    let systimer = SystemTimer::new(peripherals.SYSTIMER).split::<Target>();
    esp_hal_embassy::init(systimer.alarm0);

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiStaDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();

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

    // Initialize temperature sensor

    let sda = peripherals.GPIO10;
    let scl = peripherals.GPIO8;

    let i2c = mk_static!(
        I2c<'static, Blocking, AnyI2c>,
        I2c::new(
            peripherals.I2C0,
            Config {
                frequency: 100.kHz(),
                ..Default::default()
            }
        )
        .with_sda(sda)
        .with_scl(scl)
    );

    let sht = mk_static!(ShtCx<Sht2Gen, &'static mut I2c<'static, Blocking, AnyI2c>>, shtc3(i2c));

    let id = if cfg!(feature = "uuid-id") {
        generate_uuid_urn()
    } else {
        let device_id = stack.hardware_address().to_string();
        format!("urn:example/shtc3/{device_id}")
    };

    let td = Thing::builder("shtc3")
        .finish_extend()
        .id(id)
        .base(base_uri)
        .description("Example Thing exposing a shtc3 sensor")
        .security(|builder| builder.no_sec().required().with_key("nosec_sc"))
        .property("temperature", |p| {
            p.finish_extend_data_schema()
                .attype("TemperatureProperty")
                .title("Temperature")
                .description("Current temperature")
                .form(|f| {
                    f.href("/properties/temperature")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .number()
                .read_only()
        })
        .property("humidity", |p| {
            p.finish_extend_data_schema()
                .attype("HumidityProperty")
                .title("Humidity")
                .description("Current humidity")
                .form(|f| {
                    f.href("/properties/humidity")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .number()
                .read_only()
        })
        .event("temperatureChanged", |b| {
            b.data(|b| b.finish_extend().number().unit("degree celsius"))
                .form(|form_builder| {
                    form_builder
                        .href("/events/temperatureChanged")
                        .op(wot_td::thing::FormOperation::SubscribeEvent)
                        .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                        .subprotocol("sse")
                })
        })
        .build()
        .unwrap();

    let td = serde_json::to_string(&td).unwrap();

    sht.start_measurement(PowerMode::NormalMode).unwrap();

    let sensor = mk_static!(
            Mutex<
                CriticalSectionRawMutex,
                &'static mut
                ShtCx<
                    Sht2Gen,&'static mut
                    I2c<
                        'static,
                        Blocking,
                        AnyI2c,
                    >
                >
            >,
        Mutex::<CriticalSectionRawMutex, _>::new(sht)
    );

    let app_state = mk_static!(
        AppState,
        AppState {
            sensor,
            td: mk_static!(String, td),
        }
    );

    fn make_app() -> picoserve::Router<AppRouter, AppState> {
        picoserve::Router::new()
            .route(
                "/.well-known/wot",
                get(|State(state): State<AppState>| async move {
                    Response::ok(state.td).with_header("Content-Type", "application/td+json")
                }),
            )
            .route(
                "/properties/temperature",
                get(|State(state): State<AppState>| async move {
                    let temperature = state.get_temperature().await;

                    if let Ok(temperature) = temperature {
                        let body = format!("{}", temperature);

                        return Response::ok(body).with_header("Content-Type", "application/json");
                    }

                    Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to read temperature value.".into(),
                    )
                    .with_header("Content-Type", "text/plain")
                }),
            )
            .route(
                "/properties/humidity",
                get(|State(state): State<AppState>| async move {
                    let humidity = state.get_humidity().await;

                    if let Ok(humidity) = humidity {
                        let body = format!("{}", humidity);

                        return Response::ok(body).with_header("Content-Type", "application/json");
                    }

                    Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to read humidity value.".into(),
                    )
                    .with_header("Content-Type", "text/plain")
                }),
            )
            .route(
                "/events/temperatureChanged",
                get(move || response::EventStream(Events(WATCH.receiver().unwrap()))),
            )
    }

    let app = mk_static!(picoserve::Router<AppRouter, AppState>, make_app());

    let config = mk_static!(
        picoserve::Config::<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );

    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(id, stack, app, config, app_state));
    }

    spawner.spawn(temperature_write_task(app_state)).ok();
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}
