FROM ubuntu:20.04

WORKDIR /work

RUN sed -i 's/archive.ubuntu.com/mirrors.ustc.edu.cn/g' /etc/apt/sources.list \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y \
	    build-essential \
	    pkg-config \
	    checkinstall \
	    git \
	    autoconf \
	    automake \
	    libtool-bin \
	    libplist-dev \
        libavahi-glib-dev libavahi-client-dev \
        libimobiledevice-dev \
        libusb-1.0-0-dev \
        libssl-dev \
        udev \
        libplist++-dev libtool autoconf automake \
        python3 python3-dev \
        curl usbmuxd \
        wget lsb-release wget software-properties-common

RUN wget https://apt.llvm.org/llvm.sh \
    && chmod +x llvm.sh \
    && ./llvm.sh 14

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

RUN git clone https://github.com/jkcoxson/rusty_libimobiledevice.git --depth=1 \
    && git clone https://github.com/jkcoxson/plist_plus.git --depth=1 \
    && git clone https://github.com/libimobiledevice/libimobiledevice-glue.git --depth=1 \
    && git clone https://github.com/zeyugao/zeroconf-rs.git --depth=1 \
    && git clone https://github.com/libimobiledevice/libplist.git --depth=1 \
    && git clone https://github.com/libimobiledevice/libusbmuxd.git --depth=1

RUN cd libplist \
    && ./autogen.sh \
    && make \
    && make install 

RUN cd libimobiledevice-glue \
    && ./autogen.sh \
    && make \
    && make install

RUN cd libusbmuxd \
    && ./autogen.sh \
    && make \
    && make install

RUN . "$HOME/.cargo/env" && cargo install cargo-chef
RUN mkdir netmuxd
COPY recipe.json netmuxd
RUN . "$HOME/.cargo/env" && cd netmuxd && cargo chef cook --release --recipe-path recipe.json

COPY . netmuxd

RUN cd netmuxd \
    && . "$HOME/.cargo/env" \
    && cargo build --release
