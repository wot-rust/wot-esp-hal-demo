#![no_std]
#![no_main]
#![recursion_limit = "1024"]
#![feature(impl_trait_in_assoc_type)]
#![feature(impl_trait_in_bindings)]

extern crate alloc;

use portable_atomic::AtomicBool;

use alloc::string::String;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_println::println;
use picoserve::{
    extract::State,
    response::{self},
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

use wot_esp_thing::{mk_static, td_routes, to_json_response, EspThing as _, SseEvents, TdState};
#[derive(Clone, Copy)]
struct AppState {
    on: &'static AtomicBool,
    td: &'static embassy_sync::blocking_mutex::CriticalSectionMutex<core::cell::Cell<&'static str>>,
}

impl TdState for AppState {
    fn td(&self) -> &'static str {
        self.td.lock(|c| c.get())
    }
}

impl wot_esp_thing::EspThingState for AppState {
    fn new(
        spawner: embassy_executor::Spawner,
        peripherals: esp_hal::peripherals::Peripherals,
    ) -> (&'static Self, wot_esp_thing::NetworkPeripherals<'static>) {
        let net = wot_esp_thing::NetworkPeripherals {
            timg0: peripherals.TIMG0,
            sw_interrupt: peripherals.SW_INTERRUPT,
            wifi: peripherals.WIFI,
        };

        let app_state = mk_static!(
            AppState,
            AppState {
                on: mk_static!(AtomicBool, AtomicBool::new(false)),
                td: mk_static!(
                    embassy_sync::blocking_mutex::CriticalSectionMutex<core::cell::Cell<&'static str>>,
                    embassy_sync::blocking_mutex::CriticalSectionMutex::new(core::cell::Cell::new(""))
                ),
            }
        );

        let btn = Input::new(
            peripherals.GPIO9,
            InputConfig::default().with_pull(Pull::Up),
        );
        spawner.spawn(update_task(app_state, btn).expect("update_task"));

        (app_state, net)
    }

    fn set_td(&self, td: &'static str) {
        self.td.lock(|c| c.set(td));
    }
}

#[derive(Default)]
struct AppProps;

impl wot_esp_thing::EspThing<AppProps> for AppProps {
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
        td_routes::<AppState>()
            .route(
                "/properties/on",
                get(|State(state): State<AppState>| async move {
                    let on = state.on.load(core::sync::atomic::Ordering::Relaxed);
                    to_json_response(&on)
                }),
            )
            .route(
                "/events/on",
                get(async move || response::EventStream(SseEvents(WATCH.receiver().unwrap()))),
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

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
