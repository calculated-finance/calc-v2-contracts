#!/bin/bash

:

U="cosmwasm"
V="0.16.1"

M="x86_64" # Force Intel arch

A="linux/${M/x86_64/amd64}"
S=${M#x86_64}
S=${S:+-$S}

docker run --platform $A --rm -v "$(pwd)":/code \
--mount type=volume,source="$(basename "$(pwd)")_cache",target=/target \
--mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
$U/optimizer$S:$V
