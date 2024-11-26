# wot-esp-hal-demo

[![dependency status](https://deps.rs/repo/github/wot-rust/wot-esp-hal-demo/status.svg)](https://deps.rs/repo/github/wot-rust/wot-esp-hal-demo)
[![LICENSES][license badge apache]][license apache]
[![LICENSES][license badge mit]][license mit]

## Deploy

### Rust prerequisites
- Install `espflash`, `ldproxy` and `cargo-espflash`
```
$ cargo install espflash ldproxy cargo-espflash
```

### Building and running
- Make sure to connect the board and that its serial/jtag gets detected by your system.
- set `SSID` and `PASSWORD` env variables.

If the toolchain is correctly installed the usual `cargo build` and `cargo run` will work.

## Demos
All the demos target the [esp-rust-board](https://github.com/esp-rs/esp-rust-board).

### Hygro-Thermometer
This demo exposes the [shtc3](https://www.sensirion.com/shtc3/) sensor as a connected thermometer.

- [x] http version based on `esp-hal` and `picoserve`
- [ ] mDNS support based on `edge-mdns`

```
$ cargo run --bin thermometer
```

### Light Source
This demo exposes the **WS2812 LED** as a light source.

- [x] http version based on `esp-hal` and `picoserve`
- [ ] mDNS support based on `edge-mdns`

```
$ cargo run --bin light
```

<!-- Links -->
[license apache]: LICENSES/Apache-2.0.txt
[license mit]: LICENSES/MIT.txt

<!-- Badges -->
[license badge apache]: https://img.shields.io/badge/license-Apache_2.0-blue.svg
[license badge mit]: https://img.shields.io/badge/license-MIT-blue.svg
