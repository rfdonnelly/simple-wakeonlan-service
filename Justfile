init:
    cargo install cargo-watch
    pnpm install

dev-server:
    cargo watch -w src -w templates -w tailwind.config.js -w styles -x run

dev-tailwind:
    pnpx tailwindcss -i styles/tailwind.css -o assets/main.css --watch

dev:
    #!/bin/sh
    just dev-tailwind &
    pid1=$!
    just dev-server &
    pid2=$!
    trap "kill $pid1 pid2" EXIT
    wait $pid1 $pid2

docker-build:
    docker buildx build --platform linux/arm64 -f docker/Dockerfile -t wol .

docker-deploy:
    docker save wol | gzip | docker --host ssh://$COMPOSE_HOST load
    ssh $COMPOSE_HOST "cd $COMPOSE_PATH && docker compose up -d"
