FROM rust:alpine as builder

RUN apk add musl-dev

ENV USER=user
ENV UID=10001

RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --shell "/sbin/nologin" \
    --no-create-home \
    --uid "${UID}" \
    "${USER}"

WORKDIR /build

COPY ./ .

# FIXME: Build assets
RUN cargo build --target aarch64-unknown-linux-musl --release

FROM scratch

COPY --from=builder /etc/passwd /etc/passwd
COPY --from=builder /etc/group /etc/group

WORKDIR /run

COPY --from=builder /build/target/aarch64-unknown-linux-musl/release/simple-wakeonlan-service .
COPY --from=builder /build/assets .

USER user:user

CMD ["/run/simple-wakeonlan-service"]
