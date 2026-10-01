Source of `src/api-gateway-rs/public/og-image.png` (the 1200x630 link-preview
banner). Edit the HTML, then render it:

```bash
podman run --rm -v "$PWD/infra/og-image:/in:ro,z" -v "$PWD/src/api-gateway-rs/public:/out:z" \
  docker.io/zenika/alpine-chrome:latest --no-sandbox --hide-scrollbars --disable-gpu \
  --virtual-time-budget=4000 --screenshot=/out/og-image.png --window-size=1200,630 file:///in/og-image.html
```

Then `infra/landing-worker/deploy.sh`. Share previews (X, LinkedIn, Slack)
cache the old image for days; LinkedIn's Post Inspector refreshes it.
