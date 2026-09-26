#!/bin/sh
set -eu

# -A only generates the host key types that don't already exist, so this is a
# no-op on a container that kept its /etc/ssh (it doesn't here: recreating the
# container always gets a fresh host key, which is why setup.sh treats it as
# unstable).
ssh-keygen -A
exec /usr/sbin/sshd -D -e
