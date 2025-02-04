#!/bin/bash

set -e
shopt -s expand_aliases

if [ -z "$OS_ARCH" ]; then
	>&2 echo '$OS_ARCH is required'
	exit 1
fi

if [ -z "$ARCH" ]; then
	ARCH=$(uname -m)
fi

if [ "$0" != "./build-prod.sh" ]; then
	>&2 echo "Must be run from backend directory"
	exit 1
fi

USE_TTY=
if tty -s; then
	USE_TTY="-it"
fi

alias 'rust-gnu-builder'='docker run $USE_TTY --rm -e "OS_ARCH=$OS_ARCH" -v "$HOME/.cargo/registry":/usr/local/cargo/registry -v "$(pwd)":/home/rust/src -w /home/rust/src -P start9/rust-arm-cross:aarch64'
alias 'rust-musl-builder'='docker run $USE_TTY --rm -e "OS_ARCH=$OS_ARCH" -v "$HOME/.cargo/registry":/root/.cargo/registry -v "$(pwd)":/home/rust/src -P messense/rust-musl-cross:$ARCH-musl'

cd ..
FLAGS=""
if [[ "$ENVIRONMENT" =~ (^|-)unstable($|-) ]]; then
	FLAGS="unstable,$FLAGS"
fi
if [[ "$ENVIRONMENT" =~ (^|-)dev($|-) ]]; then
	FLAGS="dev,$FLAGS"
fi

set +e
fail=
if [[ "$FLAGS" = "" ]]; then
	rust-gnu-builder sh -c "(cd backend && cargo build --release --locked --features avahi-alias,  --target=$ARCH-unknown-linux-gnu)"
	if test $? -ne 0; then 
		fail=true
	fi
	for ARCH in x86_64 aarch64
	do
		rust-musl-builder sh -c "(cd libs && cargo build --release --locked --bin embassy_container_init )"
		if test $? -ne 0; then 
			fail=true
		fi
	done
else
	echo "FLAGS=$FLAGS"
	rust-gnu-builder sh -c "(cd backend && cargo build --release --features avahi-alias,$FLAGS --locked --target=$ARCH-unknown-linux-gnu)"
	if test $? -ne 0; then 
		fail=true
	fi
	for ARCH in x86_64 aarch64
	do
		rust-musl-builder sh -c "(cd libs && cargo build --release --features $FLAGS --locked --bin embassy_container_init)"
		if test $? -ne 0; then 
			fail=true
		fi
	done
fi
set -e
cd backend

sudo chown -R $USER target
sudo chown -R $USER ~/.cargo
sudo chown -R $USER ../libs/target

if [ -n "$fail" ]; then
	exit 1
fi
