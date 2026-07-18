fn main() {
    // WiFi credentials are baked in via env! in lib.rs; rebuild when they change.
    println!("cargo:rerun-if-env-changed=SSID");
    println!("cargo:rerun-if-env-changed=PASSWORD");
}
