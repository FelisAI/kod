# kod.felisai.pro

The marketing site. One self-contained `index.html` plus `assets/` — no build step,
no bundler, no external requests at runtime (no font CDN, no analytics, no JS).
That is deliberate: the page is small enough that a build pipeline would cost more
than it saves, and a site with no third-party requests cannot break because someone
else's CDN did.

## Local preview

```sh
cd site && python3 -m http.server 8899
# http://127.0.0.1:8899
```

## Deploying to Cloudflare Pages

`felisai.pro` is already a Cloudflare zone, so Pages creates the DNS record itself —
there is no CNAME to add by hand.

1. Cloudflare dashboard → **Workers & Pages** → **Create** → **Pages** →
   **Connect to Git** → select `FelisAI/kod`.
2. Build settings:
   - Framework preset: **None**
   - Build command: *(leave empty)*
   - **Build output directory: `site`**
3. **Save and Deploy.** The first deploy gives you a `*.pages.dev` URL.
4. That project → **Custom domains** → **Set up a custom domain** →
   `kod.felisai.pro` → **Activate**. Cloudflare writes the CNAME into the zone
   automatically and issues the certificate.

Every push to `main` redeploys. Pushes to other branches get preview URLs.

### If you would rather not connect the repo

Direct upload works too, and needs no GitHub integration:

```sh
npx wrangler pages deploy site --project-name=kod
```

## Keeping the screenshots honest

`assets/workspace.png` and `assets/standup.png` are copies of
`app/assets/screenshots/*`, which are captured from a real build against a seeded
throwaway HOME (`app/scripts/seed-demo-home.py`). They have gone stale once
already — showing a rail and an empty-state string that had both been replaced —
so **re-capture them whenever the UI in shot changes**, and copy into both places:

```sh
cp app/assets/screenshots/*.png site/assets/
```

## What is intentionally not here

- **No analytics.** Add it deliberately if you want it, knowing it is the first
  third-party request the page will make.
- **No dark/light toggle.** Kod has no light theme; a light page wrapping five dark
  screenshots reads as broken rather than considered. Single-theme is a choice.
- **No download link that works yet.** The button points at
  `github.com/FelisAI/kod/releases`, which is empty until a release carries
  `dist/Kod.dmg`. Nothing in the page needs editing when that lands.
