ARG BASE_IMAGE=docker.io/library/ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
FROM ${BASE_IMAGE}
ARG PRODUCT_VERSION
ARG SOURCE_COMMIT
LABEL org.opencontainers.image.title="vaultsyncd Linux server" \
      org.opencontainers.image.version="${PRODUCT_VERSION}" \
      org.opencontainers.image.revision="${SOURCE_COMMIT}"
# Payloads are checksum-locked before this offline build; no repository update.
COPY apt-debs/ /tmp/vaultsyncd-debs/
RUN apt-get --yes --no-install-recommends install /tmp/vaultsyncd-debs/*.deb && rm -rf /tmp/vaultsyncd-debs
COPY runtime/ /opt/vaultsyncd/
RUN groupadd --gid 8721 vaultsyncd && \
    useradd --uid 8721 --gid 8721 --no-create-home --home-dir /var/lib/vaultsyncd --shell /usr/sbin/nologin vaultsyncd && \
    install -d -m 0700 -o 8721 -g 8721 /var/lib/vaultsyncd && \
    install -d -m 0750 -o 8721 -g 8721 /srv/vaultsyncd-roots && \
    install -d -m 0755 /etc/vaultsyncd && \
    cp /opt/vaultsyncd/share/vaultsyncd/container/vaultsyncd.toml /etc/vaultsyncd/vaultsyncd.toml
USER 8721:8721
WORKDIR /var/lib/vaultsyncd
EXPOSE 8721
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8721/v1/health', timeout=4)"
ENTRYPOINT ["/opt/vaultsyncd/bin/vaultsyncd-server"]
CMD ["--config", "/etc/vaultsyncd/vaultsyncd.toml"]
