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

## AltStore
A common usecase for netmuxd is in use with [AltStore-Linux](https://github.com/NyaMisty/AltStore-Linux). 
The best way to set this up for that use case is as follows:
1. Install usbmuxd for your distribution
2. Download netmuxd from the releases and place it somewhere permanent
3. Install ``screen`` and open run a new screen like so ``screen -S netmuxd``
4. Run netmuxd like ``./netmuxd --disable-unix --host 127.0.0.1``, then press control a+d to escape the screen
5. Start a new screen for AltServer like ``screen -S altserver``
6. Set the environment variable like ``export USBMUXD_SOCKET_ADDRESS=127.0.0.1:27015``
7. Run AltServer ``./AltServer-x86_64``
