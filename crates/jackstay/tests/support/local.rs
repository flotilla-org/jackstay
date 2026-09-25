//! A connected Local Endpoint pair inside one test process.
#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::{
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use jackstay::{
    ffi_local::FtLocalConnection,
    local::{self, Connection, Endpoint, Scope, Transport},
};

/// (accepted by a listener, connected client), through a fresh endpoint.
pub fn pair() -> (Connection, Connection) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("test-pair-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let endpoint = Endpoint::new(Scope::User, &name, Transport::LocalStream).unwrap();
    let listener = local::Listener::bind(&endpoint).unwrap();
    let client = thread::spawn(move || local::connect(&endpoint).unwrap());
    let accepted = listener.accept().unwrap();
    (accepted, client.join().unwrap())
}

/// Hand a connection to the C API as an owned handle.
pub fn to_c(connection: Connection) -> *mut FtLocalConnection {
    Box::into_raw(Box::new(connection.into()))
}

/// [`pair`] as owned C handles.
pub fn c_pair() -> (*mut FtLocalConnection, *mut FtLocalConnection) {
    let (accepted, connected) = pair();
    (to_c(accepted), to_c(connected))
}
