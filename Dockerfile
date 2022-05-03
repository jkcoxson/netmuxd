FROM ubuntu:bionic-20220427 as builder

WORKDIR /work

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y \
        build-essential \
        pkg-config \
        checkinstall \
        git \
        autoconf \
        automake \
        libtool-bin \
        libavahi-glib-dev libavahi-client-dev \
        libusb-1.0-0-dev \
        libssl-dev \
        udev \
        libplist++-dev libtool autoconf automake \
        python3 python3-dev \
        curl usbmuxd \
        wget lsb-release wget software-properties-common \
        clang-10

RUN for i in /etc/ssl/certs/*.pem; do HASH=$(openssl x509 -hash -noout -in $i); ln -s $(basename $i) /etc/ssl/certs/$HASH.0 || true; done

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

RUN . "$HOME/.cargo/env" && cargo install cargo-chef

RUN git clone https://github.com/jkcoxson/rusty_libimobiledevice.git \
    && git clone https://github.com/jkcoxson/plist_plus.git \
    && git clone https://github.com/libimobiledevice/libimobiledevice-glue.git \
    && git clone https://github.com/zeyugao/zeroconf-rs.git \
    && git clone https://github.com/libimobiledevice/libplist.git \
    && git clone https://github.com/libimobiledevice/libusbmuxd.git \
    && git clone https://github.com/jkcoxson/mdns.git

RUN cd rusty_libimobiledevice && git checkout eddfea5b3aab76606f84f09de30d5066f72ae9ab && cd .. \
    && cd zeroconf-rs && git checkout 860b030064308d4318e2c6936886674d955c6472 && cd .. \
    && cd plist_plus && git checkout 7b6825f1ef89e84fd04746efec593159abec9d65 && cd .. \
    && cd mdns && git checkout 961ab21b5e01143dc3a7f0ba5f654285634e5569 && cd ..

RUN mkdir netmuxd
COPY recipe.json netmuxd
RUN . "$HOME/.cargo/env" \
    && cd netmuxd \
    && cargo chef cook --release --recipe-path recipe.json \
    && cargo chef cook --release --recipe-path recipe.json --features "zeroconf"

COPY . netmuxd

RUN mkdir -p /output/ \
    && cd netmuxd \
    && . "$HOME/.cargo/env" \
    && cargo build --release --features "zeroconf" \
    && cp target/release/netmuxd /output/netmuxd-zeroconf \
    && cargo build --release \
    && cp target/release/netmuxd /output/netmuxd-mdns

FROM ubuntu:20.04
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y \
        libavahi-client-dev

COPY --from=builder /output/ /usr/local/bin/
