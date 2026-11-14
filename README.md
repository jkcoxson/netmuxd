# netmuxd

A replacement/addition to usbmuxd which is a reimplementation of Apple's
usbmuxd on MacOS

This project used to be network-only (hence the name), but USB support was
later added.

## Building

Run ``cargo build --release`` to generate binaries. They will be generated at ``target/release/netmuxd``

## USB support

netmuxd talks to iOS devices directly over USB via nusb. There is no
dependency on a separate usbmuxd daemon: plug a device in and the daemon
will discover it and start serving the usbmuxd protocol on its Unix
socket / TCP port.

## Windows

There are two backends for Windows because Windows is a garbage operating
system with a garbage driver model.

### Apple's mux

By default, netmuxd will connect to Apple's kernel driver like Apple Mobile
Device Services does. This will evict iTunes etc. No installation is needed,
it works a lot better than libusb.

Apple's own daemon (`AppleMobileDeviceService.exe`) holds the usbmux
listener on TCP 127.0.0.1:27015.
Pass `--kill-amds` to terminate it at startup so netmuxd owns
the device and the listener. Add `--restart-amds-on-exit` to start the
`Apple Mobile Device Service` back up (via the Service Control Manager)
when netmuxd shuts down with Ctrl+C, restoring Apple's usual stack.

`--kill-amds` needs admin **every** run, because it terminates a
SYSTEM-owned service process. To pay that cost only once, take over the
service instead:

```powershell
# from an admin PowerShell, once:
.\netmuxd.exe install-service
```

`install-service` repoints the `Apple Mobile Device Service` at netmuxd
(`ChangeServiceConfig`) so the Service Control
Manager launches netmuxd as a proper Windows service

The original ImagePath is saved under `HKLM\SOFTWARE\netmuxd`; restore Apple's
binary with:

```powershell
.\netmuxd.exe uninstall-service
```

Notes:

- An Apple/iTunes update may rewrite the service back to Apple's binary. Just
  re-run `install-service`. Or maybe it doesn't, who knows. Untested.
- Under the SCM there's no console, so service-mode logs go to
  `%ProgramData%\netmuxd\netmuxd.log` (`RUST_LOG` still applies; default `info`).

### libwdi/libusb 

Apple's stock USB driver claims the iOS interface, so libusb can't open
it. netmuxd ships a one-shot installer that binds the libusb0 kernel
driver to every Apple iOS device class. Run from an **admin PowerShell**
(or admin cmd):

```powershell
.\netmuxd.exe install
```

This must be done with the device plugged in: Windows ranks Apple's
WHQL-signed INF above netmuxd's self-signed one, so the only way to win
is to force-bind via `UpdateDriverForPlugAndPlayDevices` with the device
present. If iTunes / Apple Mobile Device Support is installed, uninstall
it first and reboot. To revert, run `.\netmuxd.exe uninstall` from the
same elevated shell.

ARM64 note: Windows on ARM64 rejects libwdi's self-signed CA. Either
turn on test signing (`bcdedit /set testsigning on`) for development, or
ship the package with a Microsoft-attestation signature for production.

## Usage

Options can be listed with ``--help``

### Network devices on another subnet

If netmuxd and the device are on different subnets (for example, mDNS is
bridged by a reflector on the router), discovery will work but the device
will drop the connection, which shows up as a "broken pipe" error. The
device only accepts these connections from clients on its own subnet.

To fix this, add a source NAT rule on the router so netmuxd's traffic to the
device appears to come from the router's address on the device's subnet:

- Source: netmuxd host IP
- Destination: device IP
- Protocol: TCP, any port
- Translation: router IP on the device's subnet

## License

This code is licensed under the LGPL 2.1 license. You may use netmuxd's
code how you will, but binaries must be distributed under and with that license.
