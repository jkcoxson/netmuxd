#!/bin/bash

docker build . --build-arg http_proxy --build-arg https_proxy -t netmuxd
id=$(docker create netmuxd)
docker cp $id:/work/netmuxd/target/release/netmuxd .
docker rm -v $id
