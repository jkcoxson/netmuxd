# netmuxd

A replacement/addition to usbmuxd which is a reimplementation of Apple's usbmuxd on MacOS

# Building
Clone [rusty_libimobiledevice](https://github.com/jkcoxson/rusty_libimobiledevice), [plist_plus](https://github.com/jkcoxson/plist_plus) 
and make sure both are buildable. Instructions are in their respective readme's.

Run ``cargo build`` to generate binaries. It is located at ``target/debug/netmuxd``

Good luck, you'll need it

# Usage
You need to pair your device beforehand using another muxer like [usbmuxd](https://github.com/libimobiledevice/usbmuxd).
For example, start usbmuxd, plug in your device and enter the passcode that pops up, stop usbmuxd, start netmuxd.

Run with root, options can be listed with ``--help``

# Extension Mode
To use this project in extension with another muxer like usbmuxd, you can pass ``--disable-unix`` and ``--host 127.0.0.1``.
Then before you run a program that uses a muxer set the environment variable ``USBMUXD_SOCKET_ADDRESS=127.0.0.1:27015``.
