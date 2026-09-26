#!/bin/sh
# Generates the throwaway keypair and ssh_config the dev bastions use.
# Idempotent: safe to run before or after `docker compose up`, and safe to
# run again (e.g. after `--force-recreate bastion`).
set -eu

cd "$(dirname "$0")"
gen_dir=.generated
mkdir -p "$gen_dir"

key="$gen_dir/id_ed25519"
if [ ! -f "$key" ]; then
    ssh-keygen -t ed25519 -N "" -f "$key" -C dbdelve-dev-bastion -q
fi
chmod 600 "$key"

# World-readable: sshd's master process in the container reads this before
# dropping privileges, but on CI's Linux runner it's the runner user's UID
# that owns the file the bind mount exposes, not necessarily the container's
# root, so it has to be readable by more than its owner.
cp "$key.pub" "$gen_dir/authorized_keys"
chmod 644 "$gen_dir/authorized_keys"

abs_dir="$(pwd)/$gen_dir"
port="${DBDELVE_SSH_PORT:-52222}"

# The bastion's host key changes every time its container is recreated
# (nothing persists /etc/ssh), so a known_hosts entry from a previous run
# would make accept-new fail on the very next `docker compose up`. Clearing
# it here, on every setup run, is simpler than giving up host-key checking
# altogether for what is otherwise a real ssh session.
: > "$gen_dir/known_hosts"

cat > "$gen_dir/ssh_config" <<EOF
Host dbdelve-bastion
    HostName 127.0.0.1
    Port $port
    User dev
    IdentityFile $abs_dir/id_ed25519
    IdentitiesOnly yes
    UserKnownHostsFile $abs_dir/known_hosts
    StrictHostKeyChecking accept-new

Host dbdelve-inner
    HostName bastion2
    User dev
    IdentityFile $abs_dir/id_ed25519
    IdentitiesOnly yes
    UserKnownHostsFile $abs_dir/known_hosts
    StrictHostKeyChecking accept-new
    ProxyJump dbdelve-bastion
EOF
