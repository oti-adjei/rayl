const releasesUrl = 'https://api.github.com/repos/oti-adjei/rayl/releases';
const crateUrl = 'https://crates.io/api/v1/crates/rayl';
const archiveName = 'rayl-macos-universal.tar.gz';
const headers = { 'User-Agent': 'Rayl download counts (https://rayl.vercel.app/)' };

function downloadCount(value) {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error('Invalid download count');
  }
  return value;
}

async function githubDownloads() {
  const signal = AbortSignal.timeout(5000);
  let total = 0;
  for (let page = 1; ; page++) {
    const url = `${releasesUrl}?per_page=100&page=${page}`;
    const response = await fetch(url, { headers, signal });
    if (!response.ok) throw new Error(`GitHub returned ${response.status}`);

    const releases = await response.json();
    for (const release of releases) {
      if (release.draft) continue;
      for (const asset of release.assets) {
        if (asset.name === archiveName) total += downloadCount(asset.download_count);
      }
    }
    if (releases.length < 100) return total;
  }
}

async function cratesDownloads() {
  const response = await fetch(crateUrl, { headers, signal: AbortSignal.timeout(5000) });
  if (!response.ok) throw new Error(`crates.io returned ${response.status}`);
  const { crate } = await response.json();
  return downloadCount(crate.downloads);
}

module.exports = async function handler(req, res) {
  if (req.method !== 'GET') {
    res.setHeader('Allow', 'GET');
    return res.status(405).end();
  }

  const results = await Promise.allSettled([githubDownloads(), cratesDownloads()]);
  const [github, crates] = results.map(result => result.status === 'fulfilled' ? result.value : null);
  for (const result of results) {
    if (result.status === 'rejected') console.warn('Rayl download counts:', result.reason);
  }

  const complete = github !== null && crates !== null;
  res.setHeader('Cache-Control', complete
    ? 'public, s-maxage=900, stale-while-revalidate=3600'
    : 'public, s-maxage=60');
  return res.status(200).json({ github, crates });
};
