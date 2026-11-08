FROM alpine:3.20

# openssh-server alone gives us sshd and ssh-keygen (and busybox's nc, used by
# the compose healthcheck); no separate client package needed on the bastion.
# socat and sudo are for the Unix socket bridge: `dev` may open the Postgres
# socket compose shares in as `postgres`, and as no one else. 999 is the
# Postgres image's own uid for that user.
RUN apk add --no-cache openssh-server socat sudo && \
    adduser -D -s /bin/sh dev && \
    passwd -u dev && \
    adduser -D -H -u 999 -G nogroup -s /sbin/nologin postgres && \
    echo 'dev ALL=(postgres) NOPASSWD: /usr/bin/socat' > /etc/sudoers.d/dbdelve && \
    chmod 440 /etc/sudoers.d/dbdelve && \
    install -d -o postgres -m 3775 /run/postgresql

COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22

ENTRYPOINT ["/entrypoint.sh"]
