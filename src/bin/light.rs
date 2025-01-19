#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use alloc::string::String;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    rmt::{Channel, Rmt},
    time::RateExtU32,
    Blocking,
};
use picoserve::{
    extract::State,
    response::{Redirect, Response, StatusCode},
    routing::get,
    AppWithStateBuilder,
};

use smart_leds::{brightness, colors::WHITE, gamma, SmartLedsWrite, RGB8};
use wot_esp_hal_demo::{
    mk_static, smartLedBuffer, smartled::SmartLedsAdapter, to_json_response, EspThing as _,
};
use wot_td::{
    builder::{
        BuildableHumanReadableInfo, BuildableInteractionAffordance, IntegerDataSchemaBuilderLike,
        ObjectDataSchemaBuilderLike, SpecializableDataSchema,
    },
    Thing,
};

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
        self.update();
    }
    pub fn brightness(&mut self, b: u8) {
        self.brightness = b;
        self.update();
    }
    pub fn rgb(&mut self, rgb: RGB8) {
        self.color = rgb;
        self.update();
    }
}

#[derive(Clone, Copy)]
struct AppState {
    light: &'static Mutex<CriticalSectionRawMutex, &'static mut Light>,
    td: &'static str,
}

impl wot_esp_hal_demo::EspThingState for AppState {
    fn new(
        _spawner: embassy_executor::Spawner,
        td: String,
        peripherals: wot_esp_hal_demo::ThingPeripherals,
    ) -> &'static Self {
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

        app_state
    }
}

#[derive(Default)]
struct AppProps;

impl wot_esp_hal_demo::EspThing<AppProps> for AppProps {
    const NAME: &'static str = "light";

    fn build_td(name: &str, base_uri: String, id: String) -> Thing {
        Thing::builder(name)
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
                    Response::ok(state.td).with_header("Content-Type", "application/json")
                }),
            )
            .route("/.well-known/wot", get(|| Redirect::to("/")))
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
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
