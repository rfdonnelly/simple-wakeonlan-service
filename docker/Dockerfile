FROM rust:alpine AS builder

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

# Install tools
RUN \
    apk add --no-cache curl gcompat build-base pnpm \
    && curl -sL https://github.com/tailwindlabs/tailwindcss/releases/download/v3.4.17/tailwindcss-linux-x64 -o tailwindcss \
    && chmod +x tailwindcss

# Install deps
RUN pnpm install

# Build
RUN pnpx tailwindcss -i styles/tailwind.css -o assets/main.css --watch
RUN cargo build --target aarch64-unknown-linux-musl --release

FROM scratch

COPY --from=builder /etc/passwd /etc/passwd
COPY --from=builder /etc/group /etc/group

WORKDIR /run

COPY --from=builder /build/target/aarch64-unknown-linux-musl/release/simple-wakeonlan-service .
COPY --from=builder /build/assets ./assets

USER user:user

CMD ["/run/simple-wakeonlan-service"]
