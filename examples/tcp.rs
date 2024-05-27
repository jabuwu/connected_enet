use std::{
    io::ErrorKind,
    net::{SocketAddr, TcpListener, TcpStream},
    time::Duration,
};

fn main() {
    std::thread::spawn(|| {
        let mut host =
            connected_enet::ConnectionHost::<TcpStream>::new(enet::HostSettings::default())
                .unwrap();
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 3000))).unwrap();
        listener.set_nonblocking(true).unwrap();
        loop {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let connection = host
                            .add_connection(connected_enet::ConnectionKind::Receiver {
                                connection: stream,
                                timeout: Duration::from_secs(2),
                            })
                            .unwrap();
                        connection.set_timeout(0, 0, 0);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) => panic!("{:?}", err),
                }
            }
            while let Some(event) = host.service() {
                dbg!(event.no_ref());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        let mut host =
            connected_enet::ConnectionHost::<TcpStream>::new(enet::HostSettings::default())
                .unwrap();
        host.add_connection(connected_enet::ConnectionKind::Initiator {
            connection: TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], 3000))).unwrap(),
            channel_count: 255,
            data: 0,
        })
        .unwrap();
        loop {
            while let Some(event) = host.service() {
                match event {
                    connected_enet::Event::Connect {
                        peer: connection,
                        data: _,
                    } => {
                        connection
                            .send(0, &enet::Packet::reliable("hello".as_bytes()))
                            .unwrap();
                    }
                    event => {
                        dbg!(event.no_ref());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    loop {
        std::thread::sleep(Duration::from_millis(1));
    }
}
