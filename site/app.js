// PCompare site: product tour tabs and download links from the latest GitHub release.
const REPO = "parthh37/PCompare";

// ---------------------------------------------------------------- tour
const SHOTS = [
  ["bc5/text-compare.png", "<b>Text Compare</b>: aligned side-by-side diff with changed words highlighted. Edit either side, copy differences across and save."],
  ["context-menu.png", "<b>Folder Compare</b>: orphans, newer and different files at a glance, with content compares on demand from the context menu."],
  ["sync-preview-row-overrides.png", "<b>Folder Sync</b>: every copy, delete and new folder previewed, with per-row overrides before anything runs."],
  ["db-arrows-schema-review.jpg", "<b>Database Compare</b>: pick changes with arrows, then review them in plain English before they run."],
  ["automation-page.png", "<b>Sync Automation</b>: scheduled and real-time syncs with triggers, status, run history and alerts."],
  ["bc5/hex-aligned.png", "<b>Hex Compare</b>: byte-level differences with inserted bytes aligned, for files of any size."],
  ["bc5/archive-folder-compare.png", "<b>Archives as folders</b>: zip, tar and 7z browsed and edited in place."],
  ["araxis/git-side.png", "<b>Version control</b>: compare against <code>git:&lt;path&gt;@HEAD~3</code>, or any branch, tag or commit."],
  ["bc5/patch-viewer.png", "<b>Patch Viewer</b>: browse a <code>.patch</code> file by file and apply it to a folder."],
];

const tabs = [...document.querySelectorAll(".tour [data-shot]")];
const tourImg = document.getElementById("tour-img");
const tourCaption = document.getElementById("tour-caption");

function showShot(index) {
  const [file, caption] = SHOTS[index];
  tabs.forEach((t, i) => t.setAttribute("aria-selected", String(i === index)));
  tourCaption.innerHTML = caption;
  tourImg.alt = tourCaption.textContent;
  if (tourImg.getAttribute("src") === `screenshots/${file}`) return;
  tourImg.classList.add("fading");
  const next = new Image();
  next.onload = () => {
    tourImg.src = next.src;
    tourImg.classList.remove("fading");
  };
  next.src = `screenshots/${file}`;
}

tabs.forEach((tab, i) => tab.addEventListener("click", () => showShot(i)));
showShot(0);
// Warm the cache so switching tabs is instant.
addEventListener("load", () => SHOTS.forEach(([file]) => { new Image().src = `screenshots/${file}`; }));

// ---------------------------------------------------------------- downloads
const ASSETS = {
  "win-exe": /_x64-setup\.exe$/,
  "win-msi": /_x64_en-US\.msi$/,
  "mac-arm": /_aarch64\.dmg$/,
  "mac-x64": /_x64\.dmg$/,
};
const LABELS = {
  "win-exe": "Download for Windows",
  "mac-arm": "Download for macOS",
  "mac-x64": "Download for macOS (Intel)",
};

async function detectPlatform() {
  const uaData = navigator.userAgentData;
  const os = uaData?.platform || navigator.platform || "";
  const ua = navigator.userAgent;
  let arch;
  if (uaData?.getHighEntropyValues) {
    try {
      ({ architecture: arch } = await uaData.getHighEntropyValues(["architecture"]));
    } catch { /* not exposed: arch stays unknown */ }
  }
  if (/win/i.test(os) || /Windows/.test(ua)) return { os: "windows", arch };
  if (/mac/i.test(os) || /Macintosh/.test(ua)) return { os: "mac", arch };
  return { os: "other", arch };
}

// Which installer the hero button recommends (and the download card to highlight).
// Returns a key of ASSETS, or null to just link to the release page.
//
// `os` is "windows" | "mac" | "other"; `arch` is "arm" | "x86" | undefined.
// Only Chromium browsers report `arch`; Safari and Firefox on macOS leave it
// undefined, and Safari on Apple Silicon even claims to be an Intel Mac.
function pickRecommended({ os, arch }) {
  // TODO(you): working default; unknown Mac arch assumes Apple Silicon (every Mac sold since 2023).
  if (os === "windows") return "win-exe";
  if (os === "mac") return arch === "x86" ? "mac-x64" : "mac-arm";
  return null;
}

async function wireDownloads() {
  const platform = await detectPlatform();
  const pick = pickRecommended(platform);
  const hero = document.getElementById("hero-download");

  if (pick) {
    document.querySelector(`.dl[data-asset="${pick}"]`)?.classList.add("recommended");
    hero.querySelector("[data-dl-label]").textContent = LABELS[pick] ?? "Download PCompare";
  } else {
    hero.href = "#download";
  }

  let release;
  try {
    const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!res.ok) return; // Links already fall back to the releases page.
    release = await res.json();
  } catch {
    return;
  }

  const urlFor = (key) => release.assets.find((a) => ASSETS[key].test(a.name))?.browser_download_url;

  for (const link of document.querySelectorAll("[data-asset]")) {
    const url = urlFor(link.dataset.asset);
    if (url) link.href = url;
  }
  if (pick && urlFor(pick)) hero.href = urlFor(pick);

  const version = release.tag_name.replace(/^v/, "");
  document.querySelectorAll("[data-version]").forEach((el) => { el.textContent = `v${version}`; });
  const date = new Date(release.published_at).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
  const link = Object.assign(document.createElement("a"), { href: release.html_url, textContent: `v${version}` });
  document.querySelector("[data-release-line]").replaceChildren("Latest release ", link, `, published ${date}.`);
}

wireDownloads();
