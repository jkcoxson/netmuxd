# netmuxd

A replacement/addition to usbmuxd which is a reimplementation of Apple's usbmuxd on MacOS

# Building
Clone [rusty_libimobiledevice](https://github.com/jkcoxson/rusty_libimobiledevice), [plist_plus](https://github.com/jkcoxson/plist_plus) 
and make sure both are buildable. Instructions are in their respective readme's.

Run ``cargo build`` to generate binaries. It is located at ``target/debug/netmuxd``

Good luck, you'll need it

# Usage
Run with root, options can be listed with ``-help``
