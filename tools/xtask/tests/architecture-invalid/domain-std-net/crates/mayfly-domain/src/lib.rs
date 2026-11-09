//! Fixture: the domain opens a socket itself; `std::net` owns sockets, so even its address
//! types are reached through `core::net` instead (ADR 0037).

pub fn probe() -> bool {
    std::net::TcpStream::connect("127.0.0.1:80").is_ok()
}
