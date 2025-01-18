#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use portable_atomic::AtomicBool;

use alloc::{
    format,
    string::{String, ToString},
};
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, Pull},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_wifi::{init, wifi::WifiStaDevice, EspWifiController};
use picoserve::{
    extract::State,
    response::{self, Redirect, Response},
    routing::get,
    AppRouter, AppWithStateBuilder,
};
use wot_td::{builder::*, Thing};

use wot_esp_hal_demo::{mdns::*, *};

#[derive(Clone, Copy)]
struct AppState {
    on: &'static AtomicBool,
    td: &'static str,
}

struct AppProps;

impl AppWithStateBuilder for AppProps {
    type State = AppState;
    type PathRouter = impl picoserve::routing::PathRouter<Self::State>;

    fn build_app(self) -> picoserve::Router<Self::PathRouter, Self::State> {
        picoserve::Router::new()
            .route(
                "/",
                get(|State(state): State<AppState>| async move {
                    Response::ok(state.td).with_header("Content-Type", "application/td+json")
                }),
            )
            .route("/.well-known/wot", get(|| Redirect::to("/")))
            .route(
                "/properties/on",
                get(|State(state): State<AppState>| async move {
                    let on = state.on.load(core::sync::atomic::Ordering::Relaxed);
                    to_json_response(&on)
                }),
            )
            .route(
                "/events/on",
                get(move || response::EventStream(Events(WATCH.receiver().unwrap()))),
            )
    }
}

const WEB_TASK_POOL_SIZE: usize = 8;
// One for each http worker, 2 for mdns, 2 for internal esp-wifi use
const STACK_POOL: usize = WEB_TASK_POOL_SIZE + MDNS_STACK_SIZE + 2;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    id: usize,
    stack: Stack<'static>,
    app: &'static AppRouter<AppProps>,
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

static WATCH: Watch<CriticalSectionRawMutex, bool, 2> = Watch::new();

#[embassy_executor::task]
async fn update_task(state: &'static AppState, mut btn: Input<'static>) -> ! {
    let sender = WATCH.sender();

    loop {
        btn.wait_for_low().await;

        let on = !state.on.fetch_not(core::sync::atomic::Ordering::AcqRel);
        println!("Pressed status {on}");

        sender.send(on);
        btn.wait_for_high().await;
    }
}

struct Events<'a>(embassy_sync::watch::Receiver<'a, CriticalSectionRawMutex, bool, 2>);

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

    let rng = Rng::new(peripherals.RNG);

    esp_alloc::heap_allocator!(72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    let init = &*mk_static!(
        EspWifiController<'static>,
        init(timg0.timer0, rng, peripherals.RADIO_CLK,).unwrap()
    );

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    use esp_hal::timer::systimer::SystemTimer;
    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    esp_hal_embassy::init(systimer.alarm0);

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(
            StackResources<STACK_POOL>,
            StackResources::<STACK_POOL>::new()
        ),
        seed,
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

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

    let btn = Input::new(peripherals.GPIO9, Pull::Up);

    let id = get_urn_or_uuid(stack);

    let name = "button";

    let td = Thing::builder(name)
        .finish_extend()
        .id(id)
        .base(base_uri)
        .description("Example Thing exposing a toggle button")
        .security(|builder| builder.no_sec().required().with_key("nosec_sc"))
        .property("on", |p| {
            p.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("On if the property is true, off otherwise")
                .form(|f| {
                    f.href("/properties/on")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .bool()
                .read_only()
        })
        .event("on", |b| {
            b.data(|b| b.finish_extend().bool()).form(|form_builder| {
                form_builder
                    .href("/events/on")
                    .op(wot_td::thing::FormOperation::SubscribeEvent)
                    .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                    .subprotocol("sse")
            })
        })
        .build()
        .unwrap();

    let td = serde_json::to_string(&td).unwrap();

    let app_state = mk_static!(
        AppState,
        AppState {
            on: mk_static!(AtomicBool, AtomicBool::new(false)),
            td: mk_static!(String, td),
        }
    );

    spawner.spawn(update_task(app_state, btn)).ok();

    let app = mk_static!(AppRouter<AppProps>, AppProps.build_app());

    let config = mk_static!(
        picoserve::Config::<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );

    spawner.spawn(mdns_task(stack, rng, name)).ok();

    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(id, stack, app, config, app_state));
    }
}
