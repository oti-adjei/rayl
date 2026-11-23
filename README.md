# rayl site

Static landing page for [rayl](https://github.com/oti-adjei/rayl), deployed from this branch
(`site`). The product source lives on `main`.

## Deploying

Hosted on Vercel: import the repo, set the production branch to `site`, root directory `/`.
No frontend build step. Vercel serves the static files and deploys `api/downloads.js` as a
Node.js function. `404.html` is picked up automatically.

## Web Analytics

On the Vercel project dashboard, open **Analytics** and select **Enable**, then deploy
the `site` branch. Open the production homepage and check that
`/_vercel/insights/script.js` loads and `/_vercel/insights/view` receives a pageview.
The Analytics dashboard shows visitors and pageviews for the landing page.

The install section fetches release download counts from `/api/downloads` on page load.
Successful counts are cached at the edge for 15 minutes. On deployment, check that the
endpoint returns `github` and `crates` counts. A `null` means that source was unavailable.

## The production URL

`https://rayl.vercel.app` is written into three files — `index.html` (canonical + og:url +
JSON-LD), `robots.txt`, and `sitemap.xml`. If the domain changes (custom domain, or Vercel
assigns a different name), replace it in those files. The OG/Twitter preview image points at
`raw.githubusercontent.com` so previews work regardless of domain; optionally repoint it at
the hosted copy once the domain is final.

## Keeping AI crawlers current

`llms.txt` describes rayl for AI agents and crawlers. Keep its facts in sync with the
product README on `main` — it repeats the honesty rules: state limits, never imply sending.
