#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]
#![feature(impl_trait_in_bindings)]
#![feature(never_type)]

extern crate alloc;

use portable_atomic::AtomicBool;

use alloc::string::{String, ToString};
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_println::println;
use picoserve::{
    extract::State,
    response::{self, Redirect, Response},
    routing::get,
    AppWithStateBuilder,
};
use wot_td::{
    builder::{
        BuildableHumanReadableInfo, BuildableInteractionAffordance, ReadableWriteableDataSchema,
        SpecializableDataSchema,
    },
    Thing,
};

use wot_esp_hal_demo::{mk_static, to_json_response, EspThing as _};

#[derive(Clone, Copy)]
struct AppState {
    on: &'static AtomicBool,
    td: &'static str,
}

impl wot_esp_hal_demo::EspThingState for AppState {
    fn new(
        spawner: embassy_executor::Spawner,
        td: String,
        thing_peripherals: wot_esp_hal_demo::ThingPeripherals,
    ) -> &'static Self {
        let app_state = mk_static!(
            AppState,
            AppState {
                on: mk_static!(AtomicBool, AtomicBool::new(false)),
                td: mk_static!(String, td),
            }
        );

        let btn = Input::new(
            thing_peripherals.GPIO9,
            InputConfig::default().with_pull(Pull::Up),
        );
        spawner.spawn(update_task(app_state, btn)).ok();

        app_state
    }
}

#[derive(Default)]
struct AppProps;

impl wot_esp_hal_demo::EspThing<AppProps> for AppProps {
    const NAME: &'static str = "button";

    fn build_td(name: &str, base_uri: String, id: String) -> Thing {
        Thing::builder(name)
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
            .unwrap()
    }
}

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

impl response::sse::EventSource for Events<'_> {
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
                        .await?;
                }
                Err(_) => writer.write_keepalive().await?,
            }
        }
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
