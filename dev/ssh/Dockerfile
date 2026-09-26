FROM alpine:3.20

# openssh-server alone gives us sshd and ssh-keygen (and busybox's nc, used by
# the compose healthcheck); no separate client package needed on the bastion.
RUN apk add --no-cache openssh-server && \
    adduser -D -s /bin/sh dev && \
    passwd -u dev

COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22

ENTRYPOINT ["/entrypoint.sh"]
