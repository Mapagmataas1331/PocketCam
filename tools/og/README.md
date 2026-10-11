# Open Graph image

`og.html` is the 1200×630 social card for pocketcam.ma.cyou, styled like the site (same colors,
gradient headline, icon and real screenshots from `images/`). The rendered file is `images/og.png`,
referenced by `og:image` and `twitter:image` in `index.html`.

Re-render after changing the template or the screenshots (Chrome or Edge, from the repo root):

```sh
chrome --headless=new --hide-scrollbars --force-device-scale-factor=1 --window-size=1200,630 \
  --screenshot=images/og.png "file://$PWD/tools/og/og.html"
```

On Windows, `msedge` works the same way with `--screenshot=%CD%\images\og.png "file:///%CD%/tools/og/og.html"`.
The site uses Segoe UI, so rendering on Windows gives the closest match; elsewhere Inter is the fallback.
