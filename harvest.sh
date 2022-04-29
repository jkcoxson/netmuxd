#!/bin/bash

docker build . --build-arg http_proxy --build-arg https_proxy -t netmuxd
id=$(docker create netmuxd)
docker cp $id:/usr/local/bin/netmuxd .
docker rm -v $id
