// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io;
use std::net::SocketAddr;
use std::net::TcpListener;

use socket2::Domain;
use socket2::Socket;
use socket2::Type;

/// Own a port for the whole test, including intervals between node restarts.
///
/// Bind without listening: reserving a TLS port must not make it connectable
/// when the test never enables TLS. Once served, a clone can be passed to each
/// restarted server while this socket prevents reuse by ephemeral connections.
pub(crate) struct PortReservation(Socket);

impl PortReservation {
    pub(crate) fn new() -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
        let address: SocketAddr = "127.0.0.1:0".parse().unwrap();
        socket.bind(&address.into())?;
        Ok(Self(socket))
    }

    pub(crate) fn port(&self) -> u16 {
        self.0.local_addr().unwrap().as_socket().unwrap().port()
    }

    pub(crate) fn listener(&self) -> io::Result<TcpListener> {
        let listener = self.0.try_clone()?;
        listener.listen(1024)?;
        listener.set_nonblocking(true)?;
        Ok(listener.into())
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::net::TcpStream;

    use super::PortReservation;

    #[test]
    fn dropping_an_unserved_reservation_releases_the_port() {
        let reservation = PortReservation::new().unwrap();
        let port = reservation.port();
        drop(reservation);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn concurrent_contexts_keep_distinct_ports_owned() {
        let reservations = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..32)
                .map(|_| scope.spawn(|| PortReservation::new().unwrap()))
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        let ports: std::collections::HashSet<_> =
            reservations.iter().map(PortReservation::port).collect();
        assert_eq!(ports.len(), reservations.len());
    }

    #[test]
    fn reserved_ports_are_exclusive_but_not_yet_listening() {
        let reservation = PortReservation::new().unwrap();
        let address = ("127.0.0.1", reservation.port());
        assert!(TcpListener::bind(address).is_err());
        let socket_addr = reservation.0.local_addr().unwrap().as_socket().unwrap();
        assert!(
            TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_millis(100))
                .is_err()
        );
        let first = reservation.listener().unwrap();
        assert!(TcpStream::connect(address).is_ok());
        drop(first);
        assert!(TcpListener::bind(address).is_err());
        let restarted = reservation.listener().unwrap();
        assert_eq!(restarted.local_addr().unwrap().port(), reservation.port());
    }
}
