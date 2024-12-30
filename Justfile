init:
    cargo install cargo-watch

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
