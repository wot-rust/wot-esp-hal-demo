#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use alloc::{format, string::String};
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    prelude::*,
    rmt::{Channel, Rmt},
    rng::Rng,
    timer::timg::TimerGroup,
    Blocking,
};
use esp_println::println;
use esp_wifi::{
    init,
    wifi::{WifiDevice, WifiStaDevice},
    EspWifiController,
};
use picoserve::{
    extract::State,
    response::{Response, StatusCode},
    routing::get,
};

use smart_leds::{brightness, colors::WHITE, gamma, SmartLedsWrite, RGB8};
use wot_esp_hal_demo::{smartled::SmartLedsAdapter, *};
use wot_td::{builder::*, Thing};

struct Light {
    on: bool,
    color: RGB8,
    brightness: u8,
    led: SmartLedsAdapter<Channel<Blocking, 0>, 25>,
}

impl Light {
    fn update(&mut self) {
        let b = if self.on { self.brightness } else { 0 };
        let c = gamma([self.color].into_iter());

        self.led.write(brightness(c, b)).unwrap();
    }
    pub fn power(&mut self, on: bool) {
        self.on = on;
        self.update()
    }
    pub fn brightness(&mut self, b: u8) {
        self.brightness = b;
        self.update()
    }
    pub fn rgb(&mut self, rgb: RGB8) {
        self.color = rgb;
        self.update()
    }
}

#[derive(Clone, Copy)]
struct AppState {
    light: &'static Mutex<CriticalSectionRawMutex, &'static mut Light>,
    td: &'static str,
}

type AppRouter = impl picoserve::routing::PathRouter<AppState>;

const WEB_TASK_POOL_SIZE: usize = 1;

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

    let id = get_urn_or_uuid(stack);

    let td = Thing::builder("light")
        .finish_extend()
        .id(id)
        .base(base_uri)
        .description("Example Thing controlling a light source")
        .security(|builder| builder.no_sec().required().with_key("nosec_sc"))
        .property("on", |p| {
            p.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("The light source is on if the property is true, off otherwise")
                .form(|f| {
                    f.href("/properties/on")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .bool()
        })
        .property("brightness", |p| {
            p.finish_extend_data_schema()
                .attype("BrightnessProperty")
                .title("Light source brightness")
                .description("Light source color expressed as 8bit rgb")
                .form(|f| {
                    f.href("/properties/brightness")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .integer()
                .minimum(0)
                .maximum(255)
        })
        .property("color", |p| {
            p.finish_extend_data_schema()
                .attype("ColorProperty")
                .title("Light source color")
                .description("Light source color expressed as 8bit rgb")
                .form(|f| {
                    f.href("/properties/color")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .object()
                .property("r", true, |b| {
                    b.finish_extend()
                        .integer()
                        .title("Red")
                        .minimum(0)
                        .maximum(255)
                })
                .property("g", true, |b| {
                    b.finish_extend()
                        .integer()
                        .title("Green")
                        .minimum(0)
                        .maximum(255)
                })
                .property("b", true, |b| {
                    b.finish_extend()
                        .integer()
                        .title("Blue")
                        .minimum(0)
                        .maximum(255)
                })
        })
        .build()
        .unwrap();

    let td = serde_json::to_string(&td).unwrap();

    let rmt = Rmt::new(peripherals.RMT, 80.MHz()).unwrap();

    let rmt_buffer = smartLedBuffer!(1);

    let light = mk_static!(
        Light,
        Light {
            on: false,
            brightness: 100,
            color: WHITE,
            led: SmartLedsAdapter::new(rmt.channel0, peripherals.GPIO2, rmt_buffer)
        }
    );

    let light = mk_static!(
        Mutex<CriticalSectionRawMutex, &'static mut Light>,
        Mutex::new(light)
    );

    let app_state = mk_static!(
        AppState,
        AppState {
            light,
            td: mk_static!(String, td),
        }
    );

    fn make_app() -> picoserve::Router<AppRouter, AppState> {
        picoserve::Router::new()
            .route(
                "/.well-known/wot",
                get(|State(state): State<AppState>| async move {
                    Response::ok(state.td).with_header("Content-Type", "application/json")
                }),
            )
            .route(
                "/properties/on",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.light.lock().await.on)
                })
                .put(
                    |State(AppState { light, .. }), picoserve::extract::Json::<_, 0>(on)| async move {
                        light.lock().await.power(on);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/properties/brightness",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.light.lock().await.brightness)
                })
                .put(
                    |State(AppState { light, .. }), picoserve::extract::Json::<_, 0>(b)| async move {
                        light.lock().await.brightness(b);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/properties/color",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.light.lock().await.color)
                })
                .put(
                    |State(AppState { light, .. }), picoserve::extract::Json::<_, 0>(rgb)| async move {
                        light.lock().await.rgb(rgb);
                        StatusCode::NO_CONTENT
                    },
                ),
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
}
