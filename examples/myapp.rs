use std::{thread::sleep, time::Duration};

use minimal_logger;

use log::{debug, error, info, trace, warn};

mod db {
    use log::{debug, info};
    pub fn connect() {
        debug!("Acquiring connection from pool");
        info!("Database connected");
    }
}

mod net {
    use log::warn;
    pub fn send() {
        warn!("Retrying request — attempt 2");
    }
}

fn main() {
    minimal_logger::init().expect("Logger init failed");

    trace!("main: trace");
    debug!("main: debug");

    sleep(Duration::from_secs(5));

    info!("main: server starting on :8080");
    warn!("main: high memory usage");
    error!("main: disk full");

    db::connect();
    net::send();

    minimal_logger::shutdown();
}
