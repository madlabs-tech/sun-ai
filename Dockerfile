# syntax=docker/dockerfile:1
# Runtime image for the Sun kernel on the shared VPS (hryer/vps-infra, project
# `sun`, service `kernel`). Nothing compiles here: .github/workflows/image.yml
# cross-builds `sun` and `sun-mod-sandbox` for x86_64-unknown-linux-musl on the
# runner, copies them into dist/, and builds with dist/ as the context, so this
# file only packages them.
#
# Local build: put Linux musl `sun` and `sun-mod-sandbox` in dist/, then
# `docker build -f Dockerfile -t sun-kernel dist`
FROM alpine:3.21
RUN apk add --no-cache ca-certificates \
	&& adduser -D -u 1000 -h /var/lib/sun sun
# The kernel looks for the mod sandbox launcher next to its own executable.
COPY sun sun-mod-sandbox /usr/local/bin/
ENV SUN_DATA_DIR=/var/lib/sun HOME=/var/lib/sun
USER sun
WORKDIR /var/lib/sun
VOLUME /var/lib/sun
EXPOSE 7700
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
	CMD wget -q --spider http://127.0.0.1:7700/healthz || exit 1
ENTRYPOINT ["sun", "serve"]
# The VPS profile (same as packaging/systemd/sun.service): no emulators on the
# kernel box; device verify runs on a runtime host elsewhere.
CMD ["--listen", "0.0.0.0:7700", "--with-host", "--no-devices"]
