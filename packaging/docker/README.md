# h4rn-connect image

The relay image is built from `deploy/Dockerfile` and published to GHCR by
`.github/workflows/release.yml` on tags:

```sh
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -f deploy/Dockerfile \
  -t ghcr.io/chark1es/h4rn-connect:<version> \
  -t ghcr.io/chark1es/h4rn-connect:latest \
  --push .
```

`deploy/docker-compose.yml` is pinned to the published image tag. To build the
stack locally instead:

```sh
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.build.yml up -d --build
```

The image runs as uid 10001, exposes only `8787`, and has no compiled-in
issuer, relay URL or credentials (ADR-018).
