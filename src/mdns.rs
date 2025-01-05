use core::{
    cell::OnceCell,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use alloc::format;
use edge_mdns::{
    buf::VecBufAccess,
    domain::base::Ttl,
    host::{Host, Service, ServiceAnswers},
    io::{self, PORT},
    HostAnswersMdnsHandler,
};
use edge_nal::UdpSplit;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_net::Stack;
use embassy_sync::{
    blocking_mutex::{raw::NoopRawMutex, CriticalSectionMutex},
    signal::Signal,
};
use esp_hal::rng::Rng;
use smoltcp_012::wire::MAX_HARDWARE_ADDRESS_LEN;

static RNG: CriticalSectionMutex<OnceCell<Rng>> = CriticalSectionMutex::new(OnceCell::new());

pub const MDNS_STACK_SIZE: usize = 2;

#[embassy_executor::task]
pub async fn mdns_task(stack: Stack<'static>, rng: Rng, name: &'static str) {
    RNG.lock(|c| _ = c.set(rng.clone()));

    let ipv4 = stack.config_v4().unwrap().address.address();
    let (recv_buf, send_buf) = (
        VecBufAccess::<NoopRawMutex, 1500>::new(),
        VecBufAccess::<NoopRawMutex, 1500>::new(),
    );

    let b: UdpBuffers<MDNS_STACK_SIZE, 1500, 1500, 2> = UdpBuffers::new();

    let u = Udp::new(stack, &b);

    let mut socket = io::bind(
        &u,
        SocketAddr::new(IpAddr::V4(ipv4), PORT),
        Some(stack.config_v4().unwrap().address.address()),
        None,
    )
    .await
    .unwrap();

    let (send, recv) = socket.split();

    let hw = stack.hardware_address();
    let hw = hw.as_bytes();

    let hostname = format!(
        "{name}-{}{}{}{}",
        hw[MAX_HARDWARE_ADDRESS_LEN - 1],
        hw[MAX_HARDWARE_ADDRESS_LEN - 2],
        hw[MAX_HARDWARE_ADDRESS_LEN - 3],
        hw[MAX_HARDWARE_ADDRESS_LEN - 4]
    );

    let host = Host {
        hostname: &hostname,
        ipv4,
        ipv6: Ipv6Addr::UNSPECIFIED,
        ttl: Ttl::from_secs(60),
    };

    let service = Service {
        name,
        priority: 1,
        weight: 5,
        service: "_wot",
        protocol: "_tcp",
        port: 80,
        service_subtypes: &[],
        txt_kvs: &[
            ("td", "/.well-known/wot"),
            ("type", "Thing"),
            ("scheme", "http"),
        ],
    };

    let signal = Signal::new();

    let mdns = io::Mdns::<NoopRawMutex, _, _, _, _>::new(
        Some(ipv4),
        None,
        recv,
        send,
        recv_buf,
        send_buf,
        |buf| {
            RNG.lock(|c| c.get().map(|r| r.clone().read(buf)));
        },
        &signal,
    );

    mdns.run(HostAnswersMdnsHandler::new(ServiceAnswers::new(
        &host, &service,
    )))
    .await
    .unwrap()
}
