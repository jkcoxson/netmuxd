//! Helpers for opening an Apple iOS device's USB-mux interface and getting
//! `AsyncRead` / `AsyncWrite` halves wired up.
//!
//! Mirrors the discovery logic the daemon's nusb backend performs:
//! walk configurations highest-numbered first, find an alt setting
//! with the AMD class/subclass/protocol triple, switch to that
//! configuration only if needed, claim the interface, and
//! wrap its bulk endpoints as [`BulkReader`]/[`BulkWriter`].

use nusb::descriptors::TransferType;
use nusb::transfer::{Bulk, Direction, In, Out};
use nusb::{Device, DeviceInfo, Interface};

use crate::usb::bulk_io::{BulkReader, BulkWriter, DEFAULT_TRANSFER_SIZE};

/// Apple's USB vendor ID.
pub const APPLE_VID: u16 = 0x05ac;

/// Inclusive PID range for iPhones, iPads, and iPods that speak usbmuxd.
pub const PID_RANGE: std::ops::RangeInclusive<u16> = 0x1290..=0x12af;

/// Class/subclass/protocol triple identifying the AMD mux interface in the
/// device's configuration descriptor.
pub const MUX_INTERFACE_CLASS: u8 = 0xff;
pub const MUX_INTERFACE_SUBCLASS: u8 = 0xfe;
pub const MUX_INTERFACE_PROTOCOL: u8 = 0x02;

/// Errors from [`open_mux`].
#[derive(Debug, thiserror::Error)]
pub enum OpenMuxError {
    #[error("not an Apple usbmuxd device (vid={vid:#06x} pid={pid:#06x})")]
    NotAppleMux { vid: u16, pid: u16 },
    #[error("failed to open device: {0}")]
    Open(String),
    #[error(
        "no mux interface (class/subclass/protocol = ff/fe/02) found in any \
         configuration; device may be in a mode that doesn't expose the mux \
         pipe (e.g. iOS 17+ mode 5). Trigger a SET_MODE switch first."
    )]
    NoMuxInterface,
    #[error("failed to set configuration {config}: {error}")]
    SetConfiguration { config: u8, error: String },
    #[error(
        "failed to reset device: {error}. \
         On macOS this typically means the system usbmuxd holds an \
         exclusive session against the device."
    )]
    Reset { error: String },
    #[error(
        "failed to claim interface {interface}: {error}. \
             On macOS this often means another process holds the interface."
    )]
    ClaimInterface { interface: u8, error: String },
    #[error("failed to claim bulk IN endpoint {addr:#04x}: {error}")]
    ClaimBulkIn { addr: u8, error: String },
    #[error("failed to claim bulk OUT endpoint {addr:#04x}: {error}")]
    ClaimBulkOut { addr: u8, error: String },
}

/// True if the descriptor describes an Apple device that speaks usbmuxd.
pub fn is_apple_mux(info: &DeviceInfo) -> bool {
    info.vendor_id() == APPLE_VID && PID_RANGE.contains(&info.product_id())
}

/// Already-open device + claimed interface + bulk reader/writer halves
/// suitable for handing to [`crate::usb::mux::spawn`].
#[derive(Debug)]
pub struct OpenedMux {
    pub device: Device,
    pub interface: Interface,
    pub reader: BulkReader,
    pub writer: BulkWriter,
}

#[derive(Debug, Clone, Copy)]
struct MuxTarget {
    config_value: u8,
    interface_number: u8,
    ep_in: u8,
    ep_out: u8,
}

/// All mux-bearing configurations in the order we want to try them.
///
/// The currently-active config goes first, because trying it requires no
/// `selectConfiguration` call and so doesn't trip Linux's "another claimer
/// holds an interface" rejection. Non-active configs follow, used by retry
/// attempts that lean on macOS's IOKit ability to preempt drivers during
/// SET_CONFIGURATION.
fn collect_mux_candidates(device: &Device) -> Vec<MuxTarget> {
    let active = device
        .active_configuration()
        .ok()
        .map(|c| c.configuration_value());

    let mut all: Vec<MuxTarget> = Vec::new();
    for cfg in device.configurations() {
        for intf in cfg.interfaces() {
            for alt in intf.alt_settings() {
                if alt.class() != MUX_INTERFACE_CLASS
                    || alt.subclass() != MUX_INTERFACE_SUBCLASS
                    || alt.protocol() != MUX_INTERFACE_PROTOCOL
                {
                    continue;
                }
                let mut ep_in = None;
                let mut ep_out = None;
                for ep in alt.endpoints() {
                    if ep.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match ep.direction() {
                        Direction::In => ep_in = Some(ep.address()),
                        Direction::Out => ep_out = Some(ep.address()),
                    }
                }
                if let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) {
                    all.push(MuxTarget {
                        config_value: cfg.configuration_value(),
                        interface_number: alt.interface_number(),
                        ep_in,
                        ep_out,
                    });
                }
            }
        }
    }

    let mut ordered: Vec<MuxTarget> = all
        .iter()
        .filter(|t| Some(t.config_value) == active)
        .copied()
        .collect();
    ordered.extend(
        all.iter()
            .filter(|t| Some(t.config_value) != active)
            .copied(),
    );
    ordered
}

fn find_no_mux_config(device: &Device, mux_candidates: &[MuxTarget]) -> Option<u8> {
    let mux_configs: std::collections::HashSet<u8> =
        mux_candidates.iter().map(|t| t.config_value).collect();
    for cfg in device.configurations() {
        let v = cfg.configuration_value();
        if !mux_configs.contains(&v) {
            return Some(v);
        }
    }
    None
}

/// Multi-line descriptor dump for diagnosing claim/configuration failures.
pub fn describe_device(info: &DeviceInfo, device: &Device) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "vid={:04x} pid={:04x} serial={:?}",
        info.vendor_id(),
        info.product_id(),
        info.serial_number()
    );
    let active = device
        .active_configuration()
        .ok()
        .map(|c| c.configuration_value());
    let _ = writeln!(s, "active configuration: {:?}", active);
    for cfg in device.configurations() {
        let _ = writeln!(s, "  config {}", cfg.configuration_value());
        for intf in cfg.interfaces() {
            for alt in intf.alt_settings() {
                let _ = writeln!(
                    s,
                    "    iface #{} alt {} class={:02x}/{:02x}/{:02x}",
                    alt.interface_number(),
                    alt.alternate_setting(),
                    alt.class(),
                    alt.subclass(),
                    alt.protocol(),
                );
                for ep in alt.endpoints() {
                    let _ = writeln!(
                        s,
                        "      ep {:#04x} {:?} {:?} max_packet={}",
                        ep.address(),
                        ep.direction(),
                        ep.transfer_type(),
                        ep.max_packet_size(),
                    );
                }
            }
        }
    }
    s
}

/// Open `info`, find the mux interface in any available configuration, and
/// claim it.
///
/// Wraps [`open_mux_with_retries`] with [`DEFAULT_CLAIM_ATTEMPTS`] attempts.
pub async fn open_mux(info: &DeviceInfo) -> Result<OpenedMux, OpenMuxError> {
    open_mux_with_retries(info, DEFAULT_CLAIM_ATTEMPTS).await
}

/// Default number of claim attempts before giving up. Each attempt past the
/// first bounces through a non-mux configuration to evict any kernel
/// bindings to the mux interface before retrying.
pub const DEFAULT_CLAIM_ATTEMPTS: usize = 6;

/// Like [`open_mux`] but lets the caller pick the attempt budget.
///
/// Strategy: split the attempts in half so we cover both platforms.
///
/// - First half (no-reset / macOS-friendly path):
///   - Attempt 0 tries the active mux config without calling
///     `selectConfiguration`. The cheap path - no `SET_CONFIGURATION`
///     request is issued so the kernel/IOKit never sees EBUSY even when
///     another process holds an interface in the active config.
///   - Later attempts in this half bounce through a non-mux configuration
///     before re-asserting the mux config. On macOS this exercises
///     IOKit's "anyone with a Device handle can issue SET_CONFIGURATION
///     and preempt other claimers" behavior, which lets us steal the
///     interface from the system usbmuxd without a reset.
/// - Second half (reset / Linux-friendly path):
///   - Issue `device.reset()` first. A USB port-level reset that maps to
///     `USBDEVFS_RESET` on Linux and the equivalent IOKit ioctl on macOS.
///     The reset evicts every claimer (kernel-driver *and* libusb-based
///     userspace claimers like `usbmuxd`) and triggers re-enumeration.
///     Chrome already holds the device handle, so it gets to claim
///     before the other side's hotplug callback can reattach.
///   - If `reset()` returns an error (common on macOS, where the system
///     `usbmuxd` holds an exclusive session that blocks reset), skip the
///     rest of the attempt - a failed reset poisons the follow-up
///     `set_configuration` in Chromium's WebUSB stack, so spending the
///     attempt anyway just burns the budget without the chance of
///     succeeding.
///   - Otherwise bounce through a non-mux config to break any stale
///     kernel binding that survived, then re-assert the mux config.
pub async fn open_mux_with_retries(
    info: &DeviceInfo,
    max_attempts: usize,
) -> Result<OpenedMux, OpenMuxError> {
    if !is_apple_mux(info) {
        return Err(OpenMuxError::NotAppleMux {
            vid: info.vendor_id(),
            pid: info.product_id(),
        });
    }

    let device = info
        .open()
        .await
        .map_err(|e| OpenMuxError::Open(format!("{e:?}")))?;

    let candidates = collect_mux_candidates(&device);
    if candidates.is_empty() {
        return Err(OpenMuxError::NoMuxInterface);
    }
    let no_mux_config = find_no_mux_config(&device, &candidates);

    let attempts = max_attempts.max(1);
    let reset_threshold = attempts.div_ceil(2);
    let mut last_err: Option<OpenMuxError> = None;
    for attempt in 0..attempts {
        let use_reset = attempt >= reset_threshold;

        if use_reset
            && let Err(e) = device.reset().await
        {
            last_err = Some(OpenMuxError::Reset {
                error: format!("{e:?} (attempt {}/{attempts})", attempt + 1),
            });
            continue;
        }

        let target = candidates[attempt % candidates.len()];
        let active = device
            .active_configuration()
            .ok()
            .map(|c| c.configuration_value());

        // Skip the switch when the active config already has the mux.
        // Candidates are ordered active-first, so attempt 0 usually hits
        // this fast path. Retries that need to switch optionally bounce
        // through a non-mux config first to break any stale binding.
        let needs_switch = active != Some(target.config_value);
        if needs_switch {
            if attempt > 0
                && let Some(no_mux) = no_mux_config
            {
                let _ = device.set_configuration(no_mux).await;
            }
            if let Err(e) = device.set_configuration(target.config_value).await {
                last_err = Some(OpenMuxError::SetConfiguration {
                    config: target.config_value,
                    error: format!("{e:?} (attempt {}/{attempts})", attempt + 1),
                });
                continue;
            }
        }

        let interface = match device
            .detach_and_claim_interface(target.interface_number)
            .await
        {
            Ok(i) => i,
            Err(e) => {
                last_err = Some(OpenMuxError::ClaimInterface {
                    interface: target.interface_number,
                    error: format!("{e:?} (attempt {}/{attempts})", attempt + 1),
                });
                continue;
            }
        };

        let in_ep = interface.endpoint::<Bulk, In>(target.ep_in).map_err(|e| {
            OpenMuxError::ClaimBulkIn {
                addr: target.ep_in,
                error: format!("{e:?}"),
            }
        })?;
        let out_ep = interface
            .endpoint::<Bulk, Out>(target.ep_out)
            .map_err(|e| OpenMuxError::ClaimBulkOut {
                addr: target.ep_out,
                error: format!("{e:?}"),
            })?;

        return Ok(OpenedMux {
            device,
            interface,
            reader: BulkReader::new(in_ep, DEFAULT_TRANSFER_SIZE),
            writer: BulkWriter::new(out_ep),
        });
    }

    Err(last_err.unwrap_or(OpenMuxError::NoMuxInterface))
}
