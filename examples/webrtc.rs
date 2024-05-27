use std::{net::SocketAddr, time::Duration};

use connected_enet::{ConnectionHost, ConnectionKind};
use enet::HostSettings;
use local_ip_address::local_ip;
use simple_webrtc_channel::{Client, Configuration, DataChannelConfiguration, IceServer, Server};

pub fn webrtc_configuration() -> Configuration {
    Configuration {
        ice_servers: vec![IceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn main() {
    std::thread::spawn(|| {
        let mut server = Server::new(
            SocketAddr::from(([0, 0, 0, 0], 3000)),
            SocketAddr::from(([0, 0, 0, 0], 3001)),
            vec![local_ip().unwrap().to_string()],
            webrtc_configuration(),
        )
        .unwrap();
        let mut host = ConnectionHost::new(HostSettings::default()).unwrap();
        loop {
            if let Some(data_channel) = server.accept() {
                println!("[SERVER] Accepted connection!");
                host.add_connection(ConnectionKind::Receiver {
                    connection: data_channel,
                    timeout: Duration::from_secs(3),
                })
                .unwrap();
            }
            while let Some(event) = host.service() {
                dbg!(event.no_ref());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    std::thread::sleep(Duration::from_millis(200));
    let mut client = Client::new(
        "http://127.0.0.1:3000",
        webrtc_configuration(),
        DataChannelConfiguration::Unreliable {
            ordered: false,
            max_retransmits: 0,
        },
    );
    let data_channel = loop {
        if let Some(data_channel) = client.check_connection().unwrap() {
            break data_channel;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let mut host = ConnectionHost::new(HostSettings::default()).unwrap();
    host.add_connection(ConnectionKind::Initiator {
        connection: data_channel,
        channel_count: 1,
        data: 0,
    })
    .unwrap();
    println!("[CLIENT] Connected!");
    loop {
        while let Some(event) = host.service() {
            dbg!(event.no_ref());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
