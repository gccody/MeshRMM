# The MeshRMM server image, made from the unpacked release tarballs:
#
#   scripts/package-server.sh ... leaves dist/release/image/linux-<arch>/
#   docker buildx build --platform linux/amd64,linux/arm64 -f packaging/Dockerfile dist/release/image
#
# The server runs as an unprivileged user, so it can bind 443 only with
# Docker's default network, which lets containers bind any port. With
# --network host, run it with --user 0:0 or set http.listen to a port above
# 1023. Configure it with MESHRMM_* environment variables or a file mounted at
# /etc/meshrmm/server.toml; keep /var/lib/meshrmm on a volume.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

ARG TARGETARCH
COPY linux-${TARGETARCH}/bin/meshrmm-server /usr/bin/meshrmm-server
COPY linux-${TARGETARCH}/share/meshrmm/ /usr/share/meshrmm/
COPY linux-${TARGETARCH}/etc/meshrmm/server.example.toml /etc/meshrmm/server.example.toml
# An empty directory, so a new volume here belongs to the server's user.
COPY --chown=65532:65532 data/ /var/lib/meshrmm/

ENV MESHRMM_DATA_DIR=/var/lib/meshrmm
VOLUME ["/var/lib/meshrmm"]
EXPOSE 443/tcp 3478/udp 49160-49200/udp
USER nonroot
ENTRYPOINT ["/usr/bin/meshrmm-server"]
CMD ["serve"]
