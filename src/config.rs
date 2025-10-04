// Jackson Coxson

#[derive(Debug, Clone)]
pub struct NetmuxdConfig {
    pub port: u16,
    pub host: Option<String>,
    pub plist_storage: Option<String>,
    pub use_heartbeat: bool,
    pub use_unix: bool,
    pub use_mdns: bool,
}

impl NetmuxdConfig {
    fn default() -> Self {
        Self {
            port: 27015,
            #[cfg(unix)]
            host: None,
            #[cfg(not(unix))]
            host: Some("localhost".to_string()),
            plist_storage: None,
            use_heartbeat: true,
            use_unix: true,
            use_mdns: true,
        }
    }
    pub fn collect() -> Self {
        let mut res = Self::default();
        // Loop through args
        let mut i = 0;
        while i < std::env::args().len() {
            match std::env::args().nth(i).unwrap().as_str() {
                "-p" | "--port" => {
                    res.port = std::env::args()
                        .nth(i + 1)
                        .expect("port flag passed without number")
                        .parse::<u16>()
                        .expect("port isn't a number");
                    i += 2;
                }
                "--host" => {
                    res.host = Some(
                        std::env::args()
                            .nth(i + 1)
                            .expect("host flag passed without host")
                            .to_string(),
                    );
                    i += 2;
                }
                "--plist-storage" => {
                    res.plist_storage = Some(
                        std::env::args()
                            .nth(i + 1)
                            .expect("flag passed without value"),
                    );
                    i += 1;
                }
                #[cfg(unix)]
                "--disable-unix" => {
                    res.use_unix = false;
                    i += 1;
                }
                "--disable-mdns" => {
                    res.use_mdns = false;
                    i += 1;
                }
                "--disable-heartbeat" => {
                    res.use_heartbeat = false;
                    i += 1;
                }
                "-h" | "--help" => {
                    println!("netmuxd - a network multiplexer");
                    println!("Usage:");
                    println!("  netmuxd [options]");
                    println!("Options:");
                    println!("  -p, --port <port>");
                    println!("  --host <host>");
                    println!("  --plist-storage <path>");
                    println!("  --disable-heartbeat");
                    #[cfg(unix)]
                    println!("  --disable-unix");
                    println!("  --disable-mdns");
                    println!("  -h, --help");
                    println!("  --about");
                    println!("\n\nSet RUST_LOG to info, debug, warn, error, or trace to see more logs. Default is error.");
                    std::process::exit(0);
                }
                "--about" => {
                    println!("netmuxd - a network multiplexer");
                    println!("Copyright (c) 2020 Jackson Coxson");
                    println!("Licensed under the MIT License");
                }
                _ => {
                    i += 1;
                }
            }
        }
        res
    }
}
