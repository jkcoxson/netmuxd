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

## Windows: install the driver

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

## License

This code is licensed under the LGPL 2.1 license. You may use netmuxd's
code how you will, but binaries must be distributed under and with that license.
