use std::{convert::Infallible, time::Duration};

fn main() {
    let mut host1 = connected_enet::ConnectionHost::<enet::ReadWrite<(), Infallible>>::new(
        enet::HostSettings::default(),
    )
    .unwrap();
    let mut host2 = connected_enet::ConnectionHost::<enet::ReadWrite<(), Infallible>>::new(
        enet::HostSettings::default(),
    )
    .unwrap();

    macro_rules! update {
        () => {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(1));
                update_host("Host 1", &mut host1, &mut host2);
                update_host("Host 2", &mut host2, &mut host1);
            }
        };
    }

    host1
        .add_connection(connected_enet::ConnectionKind::Initiator {
            connection: enet::ReadWrite::new(),
            channel_count: 255,
            data: 100,
        })
        .unwrap();
    host2
        .add_connection(connected_enet::ConnectionKind::Receiver {
            connection: enet::ReadWrite::new(),
            timeout: Duration::from_secs(3),
        })
        .unwrap();

    update!();

    host1
        .peer_mut(connected_enet::ConnectionID(0))
        .send(0, &enet::Packet::reliable("Hello!".as_bytes()))
        .unwrap();

    update!();

    host1
        .peer_mut(connected_enet::ConnectionID(0))
        .disconnect(32);

    update!();

    loop {
        update!();
    }
}

fn update_host(
    name: &str,
    host: &mut connected_enet::ConnectionHost<enet::ReadWrite<(), Infallible>>,
    other_host: &mut connected_enet::ConnectionHost<enet::ReadWrite<(), Infallible>>,
) {
    while let Some(event) = host.service() {
        match event {
            connected_enet::Event::Connect { peer, data } => {
                println!(
                    "[{}] Connected to {:?} with data: {}",
                    name,
                    peer.id(),
                    data
                );
            }
            connected_enet::Event::Disconnect { peer, data } => {
                println!(
                    "[{}] Disconnected from {:?} with data: {}",
                    name,
                    peer.id(),
                    data
                );
            }
            connected_enet::Event::Receive {
                peer,
                packet,
                channel_id,
            } => {
                let message = std::str::from_utf8(packet.data()).unwrap();
                println!(
                    "[{}] Received message from {:?} on channel {}: {}",
                    name,
                    peer.id(),
                    channel_id,
                    message
                );
            }
        }
    }
    if let Some((_, packet)) = host
        .peer_mut(connected_enet::ConnectionID(0))
        .connection_mut()
        .and_then(|connection| connection.read())
    {
        if let Some(other_connection) = other_host
            .peer_mut(connected_enet::ConnectionID(0))
            .connection_mut()
        {
            other_connection.write((), packet);
        }
    }
}
