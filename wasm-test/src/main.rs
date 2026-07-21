//! Trunk demo: connect an iPhone over WebUSB, run the netmuxd usbmuxd-v2
//! protocol over the bulk pipe, and exercise the idevice library against it.
//!
//! Buttons:
//!   - Connect iPhone: trigger the WebUSB picker.
//!   - Read lockdown values: open lockdown, call `get_value(None, None)`,
//!     dump the unprotected keys.
//!   - Pair: pair against the device and stash the resulting PairingFile in
//!     `localStorage`.
//!   - TLS test (stored pairing): open a fresh lockdown connection, run
//!     `start_session` (which exercises the rustls-rustcrypto provider),
//!     call `get_value(None, None)` to dump the post-session keys.
//!   - Download / Upload pairing file: round-trip the stored PairingFile to
//!     a `.plist` on disk.

use std::cell::RefCell;

use idevice::pairing_file::PairingFile;
use idevice::remote_pairing::{RemotePairingClient, RpPairingFile};
use idevice::services::core_device::AppServiceClient;
use idevice::services::core_device_proxy::CoreDeviceProxy;
use idevice::services::rsd::RsdHandshake;
use idevice::{
    Idevice, IdeviceService, ReadWrite, RemoteXpcClient, RsdService,
    services::lockdown::LockdownClient,
};
use netmuxd::usb::apple::{self, APPLE_VID};
use netmuxd::usb::mux::UsbMuxHandle;
use netmuxd::usb::provider::UsbMuxProvider;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    Blob, BlobPropertyBag, FileReader, HtmlAnchorElement, HtmlButtonElement, HtmlElement,
    HtmlInputElement, Url, UsbDeviceFilter, UsbDeviceRequestOptions,
};

const PAIRING_STORAGE_KEY: &str = "pairing_file_xml";
const RP_PAIRING_STORAGE_KEY: &str = "rp_pairing_file_xml";
const RP_HOSTNAME: &str = "netmuxd-wasm-test";

thread_local! {
    /// Single mux handle held for the page lifetime.
    ///
    /// Opened eagerly in [`connect_iphone`] so Chrome wins the hotplug race
    /// against the system `usbmuxd` on macOS. Cloning it gives each button
    /// click its own view onto the same mux task.
    static MUX: RefCell<Option<UsbMuxHandle>> = const { RefCell::new(None) };
}

fn get_mux() -> Result<UsbMuxHandle, String> {
    MUX.with(|m| m.borrow().clone())
        .ok_or_else(|| "click \"Connect iPhone\" first".to_string())
}

fn document() -> web_sys::Document {
    web_sys::window().unwrap().document().unwrap()
}

fn log_line(s: &str) {
    web_sys::console::log_1(&JsValue::from_str(s));
    let doc = document();
    let out = doc.get_element_by_id("out").unwrap();
    let line = doc.create_element("div").unwrap();
    line.set_text_content(Some(s));
    out.append_child(&line).unwrap();
}

fn render_block(s: &str) {
    web_sys::console::log_1(&JsValue::from_str(s));
    let doc = document();
    let out = doc.get_element_by_id("out").unwrap();
    let pre = doc.create_element("pre").unwrap();
    pre.set_text_content(Some(s));
    out.append_child(&pre).unwrap();
}

fn local_storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or_else(|| "no window".to_string())?
        .local_storage()
        .map_err(|e| format!("localStorage access denied: {e:?}"))?
        .ok_or_else(|| "localStorage unavailable".to_string())
}

fn save_pairing_xml(xml: &str) -> Result<(), String> {
    local_storage()?
        .set_item(PAIRING_STORAGE_KEY, xml)
        .map_err(|e| format!("localStorage.setItem: {e:?}"))
}

fn load_pairing_xml() -> Result<Option<String>, String> {
    local_storage()?
        .get_item(PAIRING_STORAGE_KEY)
        .map_err(|e| format!("localStorage.getItem: {e:?}"))
}

fn load_pairing_file() -> Result<PairingFile, String> {
    let xml = load_pairing_xml()?
        .ok_or_else(|| "no pairing file in localStorage; pair or upload first".to_string())?;
    PairingFile::from_bytes(xml.as_bytes()).map_err(|e| format!("parse pairing file: {e:?}"))
}

/// Show the WebUSB picker, then immediately open the device, claim the mux
/// interface, and spawn the usbmuxd-v2 task. The resulting `UsbMuxHandle`
/// is stashed in [`MUX`] for the page lifetime.
///
/// Eager open (instead of opening on each button click) is the
/// macOS-reliability fix: the system `usbmuxd` aggressively claims the
/// interface on hotplug, so we want Chrome to grab it during the
/// permission-grant window and never let go. Once Chrome holds the
/// interface, system usbmuxd can't preempt it.
async fn connect_iphone() -> Result<(), String> {
    if MUX.with(|m| m.borrow().is_some()) {
        log_line("Mux already open. Reload the page to reconnect.");
        return Ok(());
    }

    let usb = web_sys::window()
        .ok_or_else(|| "no window".to_string())?
        .navigator()
        .usb();

    let filter = UsbDeviceFilter::new();
    filter.set_vendor_id(APPLE_VID);
    let filters = [filter];
    let opts = UsbDeviceRequestOptions::new(&filters);

    log_line("Requesting WebUSB device picker...");
    JsFuture::from(usb.request_device(&opts))
        .await
        .map_err(|e| format!("requestDevice: {e:?}"))?;
    log_line("Permission granted.");

    log_line("Listing devices via nusb...");
    let info = nusb::list_devices()
        .await
        .map_err(|e| format!("list_devices: {e}"))?
        .find(apple::is_apple_mux)
        .ok_or_else(|| "no Apple usbmuxd device permitted".to_string())?;

    log_line(&format!(
        "Found {:04x}:{:04x}  {}",
        info.vendor_id(),
        info.product_id(),
        info.serial_number().unwrap_or("(no serial)"),
    ));

    log_line("Opening device + claiming mux interface...");
    let opened = apple::open_mux(&info)
        .await
        .map_err(|e| format!("open_mux: {e}"))?;

    let serial = info
        .serial_number()
        .map(|s| {
            s.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string()
        })
        .unwrap_or_default();

    log_line("Spawning usbmuxd-v2 mux task...");
    let (exit_tx, _exit_rx) = tokio::sync::oneshot::channel();
    let handle =
        netmuxd::usb::mux::spawn(1, serial, opened.reader, opened.writer, exit_tx);

    MUX.with(|m| *m.borrow_mut() = Some(handle));
    log_line("Mux task ready. Click any of the other buttons to drive the device.");
    Ok(())
}

async fn open_lockdown(handle: &netmuxd::usb::mux::UsbMuxHandle) -> Result<LockdownClient, String> {
    log_line("Connecting virtual TCP to lockdownd port 62078...");
    let stream = handle
        .connect(LockdownClient::LOCKDOWND_PORT)
        .await
        .map_err(|e| format!("mux connect: {e}"))?;
    let idevice = Idevice::new(Box::new(stream), "netmuxd-wasm-test");
    Ok(LockdownClient::new(idevice))
}

async fn read_lockdown_values() -> Result<(), String> {
    let handle = get_mux()?;
    let mut lockdown = open_lockdown(&handle).await?;

    log_line("Calling lockdown.get_value(None, None)...");
    let value = lockdown
        .get_value(None, None)
        .await
        .map_err(|e| format!("get_value: {e:?}"))?;

    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &value).map_err(|e| format!("plist serialize: {e:?}"))?;
    let xml = String::from_utf8(buf).map_err(|e| format!("utf8: {e:?}"))?;

    log_line(&format!("Got {} bytes of plist:", xml.len()));
    render_block(&xml);

    Ok(())
}

async fn pair_device() -> Result<(), String> {
    let handle = get_mux()?;
    let mut lockdown = open_lockdown(&handle).await?;

    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let system_buid = uuid::Uuid::new_v4().to_string().to_uppercase();
    log_line(&format!(
        "Generated host_id={host_id} system_buid={system_buid}"
    ));

    log_line("Calling lockdown.pair() - accept the trust prompt on the device...");
    let pairing_file = lockdown
        .pair(host_id, system_buid, None)
        .await
        .map_err(|e| format!("pair: {e:?}"))?;
    log_line("Pair succeeded.");

    let serialized = pairing_file
        .clone()
        .serialize()
        .map_err(|e| format!("serialize pairing file: {e:?}"))?;
    let xml = String::from_utf8(serialized).map_err(|e| format!("utf8: {e:?}"))?;

    save_pairing_xml(&xml)?;
    log_line(&format!(
        "Pairing file ({} bytes) saved to localStorage:",
        xml.len()
    ));
    render_block(&xml);

    Ok(())
}

async fn build_provider() -> Result<UsbMuxProvider, String> {
    let pairing_file = load_pairing_file()?;
    log_line(&format!(
        "Loaded pairing file (host_id={})",
        pairing_file.host_id
    ));
    let mux = get_mux()?;
    Ok(UsbMuxProvider::new(mux, pairing_file, "netmuxd-wasm-test"))
}

/// Bring up a [`CoreDeviceProxy`] via the provider, run the CDTunnel /
/// software-TCP / RSD-handshake stack, and return the pieces shared between
/// the list-apps and rp-pair flows.
async fn rsd_handle_via_provider(
    provider: &UsbMuxProvider,
) -> Result<(idevice::tcp::handle::AdapterHandle, RsdHandshake), String> {
    log_line("CoreDeviceProxy::connect(provider)...");
    let proxy = CoreDeviceProxy::connect(provider)
        .await
        .map_err(|e| format!("CoreDeviceProxy::connect: {e:?}"))?;
    let info = proxy.tunnel_info().clone();
    log_line(&format!(
        "Tunnel up: client={} server={} mtu={} rsd_port={}",
        info.client_address, info.server_address, info.mtu, info.server_rsd_port,
    ));

    log_line("Spinning up jktcp software TCP tunnel...");
    let adapter = proxy
        .create_software_tunnel()
        .map_err(|e| format!("create_software_tunnel: {e:?}"))?;
    let mut tcp = adapter.to_async_handle();

    let rsd_stream = tcp
        .connect(info.server_rsd_port)
        .await
        .map_err(|e| format!("tcp connect rsd: {e}"))?;
    log_line("RSD handshake...");
    let handshake = RsdHandshake::new(rsd_stream)
        .await
        .map_err(|e| format!("RsdHandshake: {e:?}"))?;
    log_line(&format!(
        "RSD up: protocol={} uuid={} services={}",
        handshake.protocol_version,
        handshake.uuid,
        handshake.services.len(),
    ));

    Ok((tcp, handshake))
}

async fn list_apps() -> Result<(), String> {
    let provider = build_provider().await?;
    let (mut tcp, mut handshake) = rsd_handle_via_provider(&provider).await?;

    log_line("AppServiceClient::connect_rsd(...)");
    let mut app: AppServiceClient<Box<dyn ReadWrite>> =
        AppServiceClient::connect_rsd(&mut tcp, &mut handshake)
            .await
            .map_err(|e| format!("AppServiceClient::connect_rsd: {e:?}"))?;

    log_line("Calling list_apps(includeAll=true)...");
    let apps = app
        .list_apps(true, true, true, true, true)
        .await
        .map_err(|e| format!("list_apps: {e:?}"))?;

    log_line(&format!("{} apps:", apps.len()));
    let mut buf = String::new();
    for a in &apps {
        buf.push_str(&format!(
            "  {:<45}  {}{}\n",
            a.bundle_identifier,
            a.name,
            if a.is_first_party { "  [system]" } else { "" },
        ));
    }
    render_block(&buf);

    Ok(())
}

/// Mirror of `tools/src/rppairing.rs::pair_via_usb`, but driven from the
/// browser: stops at `RpPairingFile::to_bytes()` and stashes the result in
/// `localStorage` instead of touching the filesystem.
async fn rp_pair() -> Result<(), String> {
    let provider = build_provider().await?;
    let (mut tcp, handshake) = rsd_handle_via_provider(&provider).await?;

    let ts = handshake
        .services
        .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .ok_or_else(|| "untrusted tunnel service not advertised".to_string())?;
    log_line(&format!("Untrusted tunnel service on port {}", ts.port));

    let ts_stream = tcp
        .connect(ts.port)
        .await
        .map_err(|e| format!("tcp connect tunnelservice: {e}"))?;

    log_line("RemoteXPC handshake...");
    let mut conn = RemoteXpcClient::new(ts_stream)
        .await
        .map_err(|e| format!("RemoteXpcClient::new: {e:?}"))?;
    conn.do_handshake()
        .await
        .map_err(|e| format!("RemoteXpc do_handshake: {e:?}"))?;
    let _ = conn.recv_root().await;

    log_line("Starting RPPairing - confirm trust + enter the PIN shown on the device.");
    let mut rpf = RpPairingFile::generate(RP_HOSTNAME);
    let mut rpc = RemotePairingClient::new(conn, RP_HOSTNAME);

    rpc.connect(&mut rpf, || async {
        let win = match web_sys::window() {
            Some(w) => w,
            None => return String::new(),
        };
        match win.prompt_with_message("Enter the 6-digit PIN shown on the device") {
            Ok(Some(s)) => s.trim().to_string(),
            _ => String::new(),
        }
    })
    .await
    .map_err(|e| format!("RemotePairingClient::connect: {e:?}"))?;

    let bytes = rpf.to_bytes();
    let xml = String::from_utf8(bytes).map_err(|e| format!("rp pairing utf8: {e:?}"))?;
    save_rp_pairing_xml(&xml)?;
    log_line(&format!(
        "RPPaired! Saved {} bytes to localStorage[{}]:",
        xml.len(),
        RP_PAIRING_STORAGE_KEY
    ));
    render_block(&xml);

    Ok(())
}

fn save_rp_pairing_xml(xml: &str) -> Result<(), String> {
    local_storage()?
        .set_item(RP_PAIRING_STORAGE_KEY, xml)
        .map_err(|e| format!("localStorage.setItem: {e:?}"))
}

fn load_rp_pairing_xml() -> Result<Option<String>, String> {
    local_storage()?
        .get_item(RP_PAIRING_STORAGE_KEY)
        .map_err(|e| format!("localStorage.getItem: {e:?}"))
}

fn download_rp_pairing() -> Result<(), String> {
    let xml = load_rp_pairing_xml()?
        .ok_or_else(|| "no RPPairing file in localStorage to download".to_string())?;

    let parts = js_sys::Array::new();
    parts.push(&JsValue::from_str(&xml));
    let options = BlobPropertyBag::new();
    options.set_type("application/x-plist");
    let blob = Blob::new_with_str_sequence_and_options(&parts, &options)
        .map_err(|e| format!("Blob::new: {e:?}"))?;
    let url =
        Url::create_object_url_with_blob(&blob).map_err(|e| format!("createObjectURL: {e:?}"))?;

    let doc = document();
    let a: HtmlAnchorElement = doc
        .create_element("a")
        .map_err(|e| format!("create_element: {e:?}"))?
        .dyn_into()
        .map_err(|_| "anchor cast failed".to_string())?;
    a.set_href(&url);
    a.set_download("rp_pairing.plist");
    a.set_attribute("style", "display:none").ok();
    doc.body().unwrap().append_child(&a).ok();
    a.click();
    a.remove();
    Url::revoke_object_url(&url).ok();

    log_line(&format!(
        "Downloaded rp_pairing.plist ({} bytes).",
        xml.len()
    ));
    Ok(())
}

async fn tls_test() -> Result<(), String> {
    let pairing_file = load_pairing_file()?;
    log_line(&format!(
        "Loaded pairing file from localStorage (host_id={})",
        pairing_file.host_id
    ));

    let handle = get_mux()?;
    let mut lockdown = open_lockdown(&handle).await?;

    log_line("Calling lockdown.start_session() - runs the rustls-rustcrypto handshake...");
    lockdown
        .start_session(&pairing_file)
        .await
        .map_err(|e| format!("start_session: {e:?}"))?;
    log_line("TLS session up. Fetching get_value(None, None) - should include protected keys.");

    let value = lockdown
        .get_value(None, None)
        .await
        .map_err(|e| format!("get_value (post-session): {e:?}"))?;
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &value).map_err(|e| format!("plist serialize: {e:?}"))?;
    let xml = String::from_utf8(buf).map_err(|e| format!("utf8: {e:?}"))?;
    log_line(&format!("Post-session plist ({} bytes):", xml.len()));
    render_block(&xml);

    Ok(())
}

fn download_pairing() -> Result<(), String> {
    let xml = load_pairing_xml()?
        .ok_or_else(|| "no pairing file in localStorage to download".to_string())?;

    let parts = js_sys::Array::new();
    parts.push(&JsValue::from_str(&xml));
    let options = BlobPropertyBag::new();
    options.set_type("application/x-plist");
    let blob = Blob::new_with_str_sequence_and_options(&parts, &options)
        .map_err(|e| format!("Blob::new: {e:?}"))?;
    let url =
        Url::create_object_url_with_blob(&blob).map_err(|e| format!("createObjectURL: {e:?}"))?;

    let doc = document();
    let a: HtmlAnchorElement = doc
        .create_element("a")
        .map_err(|e| format!("create_element: {e:?}"))?
        .dyn_into()
        .map_err(|_| "anchor cast failed".to_string())?;
    a.set_href(&url);
    a.set_download("pairing.plist");
    a.set_attribute("style", "display:none").ok();
    doc.body().unwrap().append_child(&a).ok();
    a.click();
    a.remove();
    Url::revoke_object_url(&url).ok();

    log_line(&format!("Downloaded pairing.plist ({} bytes).", xml.len()));
    Ok(())
}

fn install_upload_handler(input: &HtmlInputElement) {
    let input_clone = input.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let files = match input_clone.files() {
            Some(f) => f,
            None => return,
        };
        let file = match files.get(0) {
            Some(f) => f,
            None => return,
        };
        let reader = match FileReader::new() {
            Ok(r) => r,
            Err(e) => {
                log_line(&format!("ERROR: FileReader::new: {e:?}"));
                return;
            }
        };
        let reader_clone = reader.clone();
        let onload = Closure::<dyn FnMut()>::new(move || {
            let result = match reader_clone.result() {
                Ok(r) => r,
                Err(e) => {
                    log_line(&format!("ERROR: FileReader.result: {e:?}"));
                    return;
                }
            };
            let xml = match result.as_string() {
                Some(s) => s,
                None => {
                    log_line("ERROR: FileReader did not return a string");
                    return;
                }
            };
            // Validate by round-tripping through PairingFile.
            match PairingFile::from_bytes(xml.as_bytes()) {
                Ok(_) => {
                    if let Err(e) = save_pairing_xml(&xml) {
                        log_line(&format!("ERROR: {e}"));
                        return;
                    }
                    log_line(&format!(
                        "Loaded {} bytes of pairing XML into localStorage.",
                        xml.len()
                    ));
                }
                Err(e) => log_line(&format!("ERROR: not a valid pairing file: {e:?}")),
            }
        });
        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        onload.forget();
        if let Err(e) = reader.read_as_text(&file) {
            log_line(&format!("ERROR: read_as_text: {e:?}"));
        }
    });
    input.set_onchange(Some(cb.as_ref().unchecked_ref()));
    cb.forget();
}

fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Trace);

    let doc = document();
    let body = doc.body().unwrap();

    let mk_btn = |label: &str| -> HtmlButtonElement {
        let b: HtmlButtonElement = doc.create_element("button").unwrap().dyn_into().unwrap();
        b.set_inner_text(label);
        body.append_child(&b).unwrap();
        b
    };

    let btn_connect = mk_btn("Connect iPhone");
    let btn_read = mk_btn("Read lockdown values");
    let btn_pair = mk_btn("Pair");
    let btn_tls = mk_btn("TLS test (stored pairing)");
    let btn_apps = mk_btn("List apps (CoreDeviceProxy)");
    let btn_rp_pair = mk_btn("rppair");
    let btn_rp_download = mk_btn("Download RP pairing");
    let btn_download = mk_btn("Download pairing");

    // Hidden file input + visible button to trigger it.
    let upload_input: HtmlInputElement = doc.create_element("input").unwrap().dyn_into().unwrap();
    upload_input.set_type("file");
    upload_input
        .set_attribute("accept", ".plist,application/x-plist,text/xml")
        .ok();
    upload_input.set_attribute("style", "display:none").ok();
    body.append_child(&upload_input).unwrap();
    install_upload_handler(&upload_input);

    let btn_upload = mk_btn("Upload pairing");
    {
        let upload_input = upload_input.clone();
        let cb = Closure::<dyn FnMut()>::new(move || upload_input.click());
        btn_upload.set_onclick(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    let out: HtmlElement = doc.create_element("div").unwrap().dyn_into().unwrap();
    out.set_id("out");
    body.append_child(&out).unwrap();

    // If something is already in localStorage from a previous reload, say so.
    if let Ok(Some(xml)) = load_pairing_xml() {
        log_line(&format!(
            "Found {} bytes of pairing XML in localStorage from a prior session.",
            xml.len()
        ));
    }

    let cb_connect = Closure::<dyn FnMut()>::new(move || {
        spawn_local(async move {
            if let Err(e) = connect_iphone().await {
                log_line(&format!("ERROR: {e}"));
            }
        });
    });
    btn_connect.set_onclick(Some(cb_connect.as_ref().unchecked_ref()));
    cb_connect.forget();

    let cb_read = Closure::<dyn FnMut()>::new(move || {
        spawn_local(async move {
            if let Err(e) = read_lockdown_values().await {
                log_line(&format!("ERROR: {e}"));
            }
        });
    });
    btn_read.set_onclick(Some(cb_read.as_ref().unchecked_ref()));
    cb_read.forget();

    let cb_pair = Closure::<dyn FnMut()>::new(move || {
        spawn_local(async move {
            if let Err(e) = pair_device().await {
                log_line(&format!("ERROR: {e}"));
            }
        });
    });
    btn_pair.set_onclick(Some(cb_pair.as_ref().unchecked_ref()));
    cb_pair.forget();

    let cb_tls = Closure::<dyn FnMut()>::new(move || {
        spawn_local(async move {
            if let Err(e) = tls_test().await {
                log_line(&format!("ERROR: {e}"));
            }
        });
    });
    btn_tls.set_onclick(Some(cb_tls.as_ref().unchecked_ref()));
    cb_tls.forget();

    let cb_apps = Closure::<dyn FnMut()>::new(move || {
        spawn_local(async move {
            if let Err(e) = list_apps().await {
                log_line(&format!("ERROR: {e}"));
            }
        });
    });
    btn_apps.set_onclick(Some(cb_apps.as_ref().unchecked_ref()));
    cb_apps.forget();

    let cb_rp_pair = Closure::<dyn FnMut()>::new(move || {
        spawn_local(async move {
            if let Err(e) = rp_pair().await {
                log_line(&format!("ERROR: {e}"));
            }
        });
    });
    btn_rp_pair.set_onclick(Some(cb_rp_pair.as_ref().unchecked_ref()));
    cb_rp_pair.forget();

    let cb_rp_download = Closure::<dyn FnMut()>::new(move || {
        if let Err(e) = download_rp_pairing() {
            log_line(&format!("ERROR: {e}"));
        }
    });
    btn_rp_download.set_onclick(Some(cb_rp_download.as_ref().unchecked_ref()));
    cb_rp_download.forget();

    let cb_download = Closure::<dyn FnMut()>::new(move || {
        if let Err(e) = download_pairing() {
            log_line(&format!("ERROR: {e}"));
        }
    });
    btn_download.set_onclick(Some(cb_download.as_ref().unchecked_ref()));
    cb_download.forget();
}
