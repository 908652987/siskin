FROM debian:bookworm-slim

RUN sed -i 's|deb.debian.org|mirrors.ustc.edu.cn|g' /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends dovecot-imapd \
 && rm -rf /var/lib/apt/lists/* \
 && (getent group 20 >/dev/null || groupadd -g 20 staff) \
 && useradd -u 501 -g 20 -m -d /home/vmail -s /bin/sh vmail

COPY dovecot.conf /etc/dovecot/dovecot.conf
COPY dovecot-passwd /etc/dovecot/users

EXPOSE 143
CMD ["dovecot", "-F"]
