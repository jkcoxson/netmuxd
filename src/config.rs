// Jackson Coxson

use idevice::usbmuxd::UsbmuxdAddr;

#[cfg(unix)]
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/usbmuxd";

#[derive(Debug, Clone)]
pub struct NetmuxdConfig {
    pub port: u16,
    pub host: Option<String>,
    pub plist_storage: Option<String>,
    pub use_heartbeat: bool,
    #[cfg(unix)]
    pub use_unix: bool,
    pub use_mdns: bool,
    pub use_usb: bool,
    pub apple_mux: bool,
    pub upstream: Option<UsbmuxdAddr>,
    #[cfg(unix)]
    pub socket_path: String,
}

impl NetmuxdConfig {
    fn default() -> Self {
        Self {
            port: 27015,
            #[cfg(unix)]
            host: None,
            #[cfg(not(unix))]
            host: Some("127.0.0.1".to_string()),
            plist_storage: None,
            use_heartbeat: true,
            #[cfg(unix)]
            use_unix: true,
            use_mdns: true,
            use_usb: true,
            apple_mux: true,
            upstream: None,
            #[cfg(unix)]
            socket_path: DEFAULT_SOCKET_PATH.to_string(),
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
                "--disable-usb" => {
                    res.use_usb = false;
                    i += 1;
                }
                #[cfg(all(windows, feature = "libusbk"))]
                "--libusbk" => {
                    res.apple_mux = false;
                    i += 1;
                }
                "--disable-heartbeat" => {
                    res.use_heartbeat = false;
                    i += 1;
                }
                "--upstream-usbmuxd" => {
                    match std::env::args().nth(i + 1) {
                        Some(addr) if !addr.starts_with('-') => {
                            res.upstream = Some(parse_upstream(&addr));
                            i += 2;
                        }
                        _ => {
                            res.upstream = Some(UsbmuxdAddr::from_env_var().unwrap_or_default());
                            i += 1;
                        }
                    }
                    res.use_usb = false;
                }
                #[cfg(unix)]
                "--socket-path" => {
                    res.socket_path = std::env::args()
                        .nth(i + 1)
                        .expect("--socket-path passed without a path");
                    i += 2;
                }
                "-h" | "--help" => {
                    println!("netmuxd - a network multiplexer");
                    println!("Usage:");
                    #[cfg(unix)]
                    println!("  netmuxd [options]");
                    #[cfg(all(windows, feature = "libusbk"))]
                    {
                        println!("  netmuxd [argument] [options]");
                        println!("Arguments:");
                        println!("  install (installs the libusbK driver)");
                        println!("  uninstall (uninstalls the libusbK driver)");
                        println!("  export-driver (exports the driver files for signing)");
                    }
                    #[cfg(all(windows, not(feature = "libusbk")))]
                    println!("  netmuxd [options]");
                    println!("Options:");
                    println!("  -p, --port <port>");
                    println!("  --host <host>");
                    println!("  --plist-storage <path>");
                    println!("  --disable-heartbeat");
                    #[cfg(unix)]
                    println!("  --disable-unix");
                    println!("  --disable-mdns");
                    println!("  --disable-usb");
                    #[cfg(all(windows, feature = "libusbk"))]
                    {
                        println!(
                            "  --libusbk                  (Windows: use the legacy libusbK backend instead of the"
                        );
                        println!(
                            "                              default Apple-driver backend. Requires libusbK.dll and the"
                        );
                        println!(
                            "                              netmuxd-installed driver, see the `install` command. By"
                        );
                        println!(
                            "                              default netmuxd drives iOS devices through Apple's installed"
                        );
                        println!(
                            "                              WinUSB stack, which needs no libusbK.dll but requires Apple's"
                        );
                        println!(
                            "                              Mobile Device Support / Apple Devices app installed.)"
                        );
                    }
                    println!(
                        "  --upstream-usbmuxd [addr]  (shim mode: forward USB/most requests to this muxer;"
                    );
                    println!(
                        "                              addr is a unix socket path or IP:port, defaulting to the"
                    );
                    println!(
                        "                              system usbmuxd / USBMUXD_SOCKET_ADDRESS; implies --disable-usb)"
                    );
                    #[cfg(unix)]
                    println!(
                        "  --socket-path <path>       (unix socket to listen on; default {DEFAULT_SOCKET_PATH})"
                    );
                    println!("  -h, --help");
                    println!("  --about");
                    println!(
                        "\n\nSet RUST_LOG to info, debug, warn, error, or trace to see more logs. Default is error."
                    );
                    std::process::exit(0);
                }
                "--about" => {
                    println!(
                        "netmuxd v{} - a network multiplexer",
                        env!("CARGO_PKG_VERSION")
                    );
                    println!("Copyright (c) 2020 Jackson Coxson");
                    println!("Licensed under the MIT License");
                    std::process::exit(0);
                }
                _ => {
                    i += 1;
                }
            }
        }
        res
    }
}

/// Parse an `--upstream-usbmuxd` value into a [`UsbmuxdAddr`].
///
/// On Unix a value without a `:` is treated as a socket path; otherwise it's
/// parsed as an `IP:port` TCP address. On non-Unix only TCP is supported.
fn parse_upstream(addr: &str) -> UsbmuxdAddr {
    #[cfg(unix)]
    {
        if addr.contains(':') {
            UsbmuxdAddr::TcpSocket(
                addr.parse()
                    .expect("--upstream-usbmuxd TCP address must be IP:port"),
            )
        } else {
            UsbmuxdAddr::UnixSocket(addr.to_string())
        }
    }
    #[cfg(not(unix))]
    {
        UsbmuxdAddr::TcpSocket(
            addr.parse()
                .expect("--upstream-usbmuxd TCP address must be IP:port"),
        )
    }
}
