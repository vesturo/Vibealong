#!/usr/bin/env node

import { mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { performance } from "node:perf_hooks";

const REDIRECTS = new Set([301, 302, 303, 307, 308]);
const WEIGHT = { critical: 0, high: 1, medium: 2, low: 3, info: 4 };
const DEFAULTS = {
  maxPages: 80,
  timeoutMs: 20000,
  sitemapSample: 20,
  includeQuery: false,
  respectRobots: false,
  out: `reports/seo-report-${new Date().toISOString().replace(/[:.]/g, "-")}.md`,
  seedPaths: ["/", "/browse", "/live", "/studios", "/privacy", "/terms", "/contact"],
  userAgent:
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
    "(KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36 VRLewdsSEOAudit/1.0",
};

function help() {
  const lines = [
    "Usage: node scripts/seo-audit.mjs --site https://dev.vrlewds.com [options]",
    "",
    "Options:",
    "  --site <url>             Target URL/origin (or use positional URL)",
    "  --out <path>             Report output path",
    "  --max-pages <n>          Crawl budget (default: 80)",
    "  --timeout-ms <n>         Request timeout (default: 20000)",
    "  --seed-paths <csv>       Extra seed paths (default includes core pages)",
    "  --sitemap-sample <n>     Sitemap URLs to validate (default: 20)",
    "  --include-query          Keep query params while crawling",
    "  --respect-robots         Skip wildcard-disallowed paths",
    "  --help                   Show this help",
  ];
  process.stdout.write(`${lines.join("\n")}\n`);
}

function toInt(v, flag) {
  const n = Number(v);
  if (!Number.isFinite(n) || n <= 0) throw new Error(`Invalid ${flag}: ${v}`);
  return Math.floor(n);
}

function parseArgs(argv) {
  const o = { ...DEFAULTS };
  let positional = "";
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === "--help" || a === "-h") {
      help();
      process.exit(0);
    }
    if (!a.startsWith("--")) {
      if (!positional) positional = a;
      else throw new Error(`Unexpected argument: ${a}`);
      continue;
    }
    const next = () => {
      const v = argv[i + 1];
      if (!v || v.startsWith("--")) throw new Error(`Missing value for ${a}`);
      i += 1;
      return v;
    };
    if (a === "--site") o.site = next();
    else if (a === "--out") o.out = next();
    else if (a === "--max-pages") o.maxPages = toInt(next(), a);
    else if (a === "--timeout-ms") o.timeoutMs = toInt(next(), a);
    else if (a === "--sitemap-sample") o.sitemapSample = toInt(next(), a);
    else if (a === "--seed-paths") o.seedPaths = next().split(",").map((x) => x.trim()).filter(Boolean);
    else if (a === "--include-query") o.includeQuery = true;
    else if (a === "--respect-robots") o.respectRobots = true;
    else throw new Error(`Unknown flag: ${a}`);
  }
  o.site = o.site || positional;
  if (!o.site) throw new Error("Missing --site <url>.");
  return o;
}

function siteUrl(input) {
  let v = input.trim();
  if (!/^https?:\/\//i.test(v)) v = `https://${v}`;
  const u = new URL(v);
  u.hash = "";
  u.search = "";
  if (!u.pathname) u.pathname = "/";
  return u;
}

function norm(raw, includeQuery) {
  const u = new URL(raw);
  u.hash = "";
  if (!includeQuery) u.search = "";
  if (u.pathname.length > 1 && u.pathname.endsWith("/")) u.pathname = u.pathname.replace(/\/+$/g, "");
  return u.toString();
}

function toAbs(base, href) {
  if (!href) return null;
  const h = href.trim();
  if (!h || h.startsWith("#") || /^(javascript|mailto|tel|data):/i.test(h)) return null;
  try {
    const u = new URL(h, base);
    return /^https?:$/i.test(u.protocol) ? u.toString() : null;
  } catch {
    return null;
  }
}

function skipCrawl(url) {
  const p = url.pathname.toLowerCase();
  if (p.startsWith("/_next/") || p.startsWith("/cdn-cgi/") || p.startsWith("/api/")) return true;
  return /\.(?:png|jpe?g|gif|svg|webp|ico|pdf|zip|mp4|mkv|mov|css|js|map|xml|txt|woff2?|ttf|eot)$/i.test(p);
}

function attrs(tag) {
  const out = {};
  const re = /([^\s=/>]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))/g;
  let m = re.exec(tag);
  while (m) {
    out[m[1].toLowerCase()] = m[2] ?? m[3] ?? m[4] ?? "";
    m = re.exec(tag);
  }
  return out;
}

function decode(s) {
  return (s || "")
    .replace(/&nbsp;/gi, " ")
    .replace(/&amp;/gi, "&")
    .replace(/&quot;/gi, '"')
    .replace(/&#39;/gi, "'")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">");
}

function strip(s) {
  return decode((s || "").replace(/<[^>]*>/g, " "));
}

function pageSignals(html, pageUrl, origin, includeQuery) {
  const title = strip((html.match(/<title[^>]*>([\s\S]*?)<\/title>/i) || [])[1]).trim();
  const metas = html.match(/<meta\b[^>]*>/gi) || [];
  const links = html.match(/<link\b[^>]*>/gi) || [];
  const images = html.match(/<img\b[^>]*>/gi) || [];
  const anchors = [...html.matchAll(/<a\b[^>]*\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))[^>]*>/gi)];
  const byName = {};
  const byProp = {};
  for (const t of metas) {
    const a = attrs(t);
    if (a.name) byName[a.name.toLowerCase()] = (a.content || "").trim();
    if (a.property) byProp[a.property.toLowerCase()] = (a.content || "").trim();
  }
  let canonical = "";
  let hreflangCount = 0;
  for (const t of links) {
    const a = attrs(t);
    const rel = (a.rel || "").toLowerCase();
    if (!canonical && rel.includes("canonical") && a.href) canonical = a.href.trim();
    if (rel.includes("alternate") && a.hreflang && a.href) hreflangCount += 1;
  }
  const internal = new Set();
  const external = new Set();
  let mixed = 0;
  for (const m of anchors) {
    const href = m[1] ?? m[2] ?? m[3] ?? "";
    const abs = toAbs(pageUrl, href);
    if (!abs) continue;
    const u = new URL(abs);
    if (u.protocol === "http:" && new URL(pageUrl).protocol === "https:") mixed += 1;
    if (u.origin === origin) internal.add(norm(abs, includeQuery));
    else external.add(abs);
  }
  let missingAlt = 0;
  for (const t of images) if (!Object.prototype.hasOwnProperty.call(attrs(t), "alt")) missingAlt += 1;
  const text = strip(html.replace(/<script[\s\S]*?<\/script>/gi, " ").replace(/<style[\s\S]*?<\/style>/gi, " "));
  const words = text.split(/\s+/).filter(Boolean).length;
  const h1 = (html.match(/<h1\b/gi) || []).length;
  const h2 = (html.match(/<h2\b/gi) || []).length;
  const jsonLd = (html.match(/<script\b[^>]*type=["']application\/ld\+json["'][^>]*>/gi) || []).length;
  const lang = (attrs(`<html ${(html.match(/<html\b([^>]*)>/i) || [])[1] || ""}>`).lang || "").trim();
  return {
    title,
    description: byName.description || "",
    robots: byName.robots || "",
    canonical,
    hreflangCount,
    h1,
    h2,
    words,
    viewport: byName.viewport || "",
    lang,
    ogTitle: byProp["og:title"] || "",
    ogDescription: byProp["og:description"] || "",
    ogImage: byProp["og:image"] || "",
    twitterCard: byName["twitter:card"] || "",
    jsonLd,
    images: images.length,
    missingAlt,
    mixed,
    internalLinks: [...internal],
    externalLinks: [...external],
    challenge: /cdn-cgi\/challenge|challenge-platform\/scripts/i.test(html),
  };
}

async function request(url, options, method = "GET") {
  const start = performance.now();
  try {
    const res = await fetch(url, {
      method,
      redirect: "manual",
      signal: AbortSignal.timeout(options.timeoutMs),
      headers: { "user-agent": options.userAgent, "accept-language": "en-US,en;q=0.9", accept: "*/*" },
    });
    return { ok: true, res, ms: Math.round(performance.now() - start) };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e), ms: Math.round(performance.now() - start) };
  }
}

async function fetchUrl(url, options, readBody = true) {
  const requested = norm(url, options.includeQuery);
  let current = requested;
  const chain = [];
  let totalMs = 0;
  for (let i = 0; i < 8; i += 1) {
    const r = await request(current, options, "GET");
    totalMs += r.ms;
    if (!r.ok) return { requested, final: current, status: 0, error: r.error, chain, totalMs, headers: {}, ct: "", body: "" };
    const status = r.res.status;
    const loc = r.res.headers.get("location") || "";
    chain.push({ url: current, status, location: loc, ms: r.ms });
    if (REDIRECTS.has(status) && loc) {
      try {
        current = norm(new URL(loc, current).toString(), options.includeQuery);
        continue;
      } catch {
        return { requested, final: current, status, error: `Invalid redirect: ${loc}`, chain, totalMs, headers: Object.fromEntries(r.res.headers.entries()), ct: r.res.headers.get("content-type") || "", body: "" };
      }
    }
    const ct = r.res.headers.get("content-type") || "";
    const body =
      readBody && /text\/|json|xml|javascript|html/i.test(ct) ? await r.res.text() : "";
    return { requested, final: current, status, error: "", chain, totalMs, headers: Object.fromEntries(r.res.headers.entries()), ct, body };
  }
  return { requested, final: current, status: 0, error: "Too many redirects", chain, totalMs, headers: {}, ct: "", body: "" };
}

function parseRobots(body) {
  const rules = [];
  const sitemaps = [];
  let inWildcard = false;
  for (const raw of body.split(/\r?\n/)) {
    const line = raw.split("#")[0].trim();
    if (!line) continue;
    const i = line.indexOf(":");
    if (i < 1) continue;
    const k = line.slice(0, i).trim().toLowerCase();
    const v = line.slice(i + 1).trim();
    if (k === "user-agent") inWildcard = v.toLowerCase() === "*";
    else if (k === "sitemap" && v) sitemaps.push(v);
    else if ((k === "allow" || k === "disallow") && inWildcard) rules.push({ type: k, path: v });
  }
  return { rules, sitemaps };
}

function blocked(pathname, rules) {
  let best = null;
  for (const r of rules) {
    if (!r.path) continue;
    const regex = new RegExp(`^${r.path.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\$$/, "$")}`);
    if (!regex.test(pathname)) continue;
    const score = r.path.length;
    if (!best || score > best.score || (score === best.score && r.type === "allow")) best = { ...r, score };
  }
  return best ? best.type === "disallow" : false;
}

async function crawl(start, options, rules) {
  const startUrl = new URL(start);
  const origin = startUrl.origin;
  const queue = [];
  const seen = new Set();
  const queued = new Set();
  const pages = [];
  const push = (raw) => {
    try {
      const u = new URL(raw, origin);
      if (u.origin !== origin || skipCrawl(u)) return;
      const n = norm(u.toString(), options.includeQuery);
      if (!seen.has(n) && !queued.has(n)) {
        queued.add(n);
        queue.push(n);
      }
    } catch {}
  };
  push(startUrl.toString());
  for (const p of options.seedPaths) push(p);
  while (queue.length && pages.length < options.maxPages) {
    const url = queue.shift();
    if (!url || seen.has(url)) continue;
    seen.add(url);
    if (options.respectRobots && blocked(new URL(url).pathname, rules)) {
      pages.push({ requested: url, final: url, status: 0, error: "Skipped by robots.txt", isHtml: false, signals: null, headers: {}, ms: 0, redirects: [] });
      continue;
    }
    process.stderr.write(`[seo-audit] ${pages.length + 1}/${options.maxPages} ${url}\n`);
    const r = await fetchUrl(url, options, true);
    const isHtml = !r.error && (/text\/html|application\/xhtml\+xml/i.test(r.ct) || r.body.trimStart().toLowerCase().startsWith("<!doctype html"));
    const page = {
      requested: url,
      final: r.final,
      status: r.status,
      error: r.error,
      isHtml,
      ct: r.ct,
      headers: r.headers,
      ms: r.totalMs,
      redirects: r.chain,
      signals: null,
    };
    if (isHtml && r.body) {
      page.signals = pageSignals(r.body, r.final, origin, options.includeQuery);
      for (const t of page.signals.internalLinks) push(t);
    }
    if (norm(r.final, options.includeQuery) !== url) push(r.final);
    pages.push(page);
  }
  return { pages, truncated: queue.length > 0 };
}

function issue(severity, scope, summary, fix, urls = [], details = "") {
  return { severity, scope, summary, fix, urls, details };
}

function analyze(crawlData, robotsRes, sitemapRes, options) {
  const pages = crawlData.pages;
  const issues = [];
  const perPage = new Map();
  const host = new URL(options.site).hostname.toLowerCase();
  const devHost = host.startsWith("dev.") || host.startsWith("staging.") || host.includes("preview");
  const add = (u, i) => {
    issues.push(i);
    if (!u) return;
    const arr = perPage.get(u) || [];
    arr.push(i);
    perPage.set(u, arr);
  };
  if (robotsRes.status !== 200) add("", issue(robotsRes.status === 404 ? "medium" : "high", "site", "robots.txt missing or inaccessible", "Expose robots.txt and ensure bot access."));
  if (!sitemapRes.urls.length) add("", issue("high", "site", "No sitemap URLs discovered", "Publish sitemap.xml or sitemap index with <url><loc> entries."));
  const sBad = sitemapRes.sample.filter((x) => x.status >= 400 || x.status === 0).length;
  if (sBad) add("", issue("medium", "site", `${sBad} sitemap sample URL(s) returned errors`, "Fix stale sitemap entries and keep sitemap in sync."));
  if (crawlData.truncated) add("", issue("medium", "site", `Crawl hit max-pages (${options.maxPages})`, "Raise --max-pages and rerun."));

  const byReq = new Map(pages.map((p) => [p.requested, p]));
  const byFinal = new Map(pages.map((p) => [norm(p.final, options.includeQuery), p]));
  const titleMap = new Map();
  const descMap = new Map();
  let html200 = 0;
  let indexable = 0;
  let noindex = 0;

  for (const p of pages) {
    if (p.error) add(p.requested, issue("high", "page", "Fetch error", "Fix route availability for crawlers.", [p.requested], p.error));
    else if (p.status >= 500) add(p.requested, issue("high", "page", `HTTP ${p.status}`, "Resolve server-side errors.", [p.requested]));
    else if (p.status >= 400) add(p.requested, issue("high", "page", `HTTP ${p.status}`, "Fix broken pages or remove links to them.", [p.requested]));
    if (!(p.status === 200 && p.isHtml && p.signals)) continue;
    html200 += 1;
    const s = p.signals;
    if (!s.title) add(p.requested, issue("high", "page", "Missing <title>", "Add unique title tags.", [p.requested]));
    else if (s.title.length < 25 || s.title.length > 65) add(p.requested, issue("medium", "page", `Title length ${s.title.length}`, "Target 25-65 chars.", [p.requested]));
    if (!s.description) add(p.requested, issue("medium", "page", "Missing meta description", "Add unique descriptions (~70-160 chars).", [p.requested]));
    else if (s.description.length < 70 || s.description.length > 170) add(p.requested, issue("low", "page", `Description length ${s.description.length}`, "Target ~70-160 chars.", [p.requested]));
    if (!s.canonical) add(p.requested, issue("medium", "page", "Missing canonical tag", "Add rel=canonical on indexable pages.", [p.requested]));
    if (s.h1 === 0) add(p.requested, issue("high", "page", "Missing H1", "Add one primary H1 per page.", [p.requested]));
    else if (s.h1 > 1) add(p.requested, issue("medium", "page", `Multiple H1s (${s.h1})`, "Prefer a single primary H1.", [p.requested]));
    if (!s.viewport) add(p.requested, issue("medium", "page", "Missing viewport meta", "Add mobile viewport meta tag.", [p.requested]));
    if (!s.lang) add(p.requested, issue("low", "page", "Missing html lang", "Set <html lang=\"...\">.", [p.requested]));
    if (!s.ogTitle || !s.ogDescription || !s.ogImage) add(p.requested, issue((!s.ogTitle && !s.ogDescription && !s.ogImage) ? "medium" : "low", "page", "OpenGraph tags incomplete", "Add og:title, og:description, og:image.", [p.requested]));
    if (!s.twitterCard) add(p.requested, issue("low", "page", "Missing twitter:card", "Add twitter card metadata.", [p.requested]));
    if (s.jsonLd === 0) add(p.requested, issue("low", "page", "No JSON-LD detected", "Add relevant schema.org JSON-LD.", [p.requested]));
    if (s.missingAlt > 0) add(p.requested, issue("medium", "page", `${s.missingAlt} images missing alt`, "Add alt text (or empty alt for decorative images).", [p.requested]));
    if (s.mixed > 0) add(p.requested, issue("high", "page", `${s.mixed} mixed-content links`, "Use HTTPS for all links.", [p.requested]));
    if (s.words < 80) add(p.requested, issue("low", "page", `Low text content (~${s.words} words)`, "Increase crawlable content on key pages.", [p.requested]));
    if (s.challenge) add(p.requested, issue("high", "page", "Bot challenge signatures detected", "Allow trusted crawlers through anti-bot controls.", [p.requested]));
    const xRobots = p.headers["x-robots-tag"] || "";
    if (/\bnoindex\b/i.test(s.robots) || /\bnoindex\b/i.test(xRobots)) {
      noindex += 1;
      add(p.requested, issue(devHost ? "info" : "high", "page", "Page has noindex", devHost ? "Keep in dev, remove before production launch." : "Remove noindex on pages meant to rank.", [p.requested], `robots=${s.robots || "n/a"} x-robots-tag=${xRobots || "n/a"}`));
    } else indexable += 1;
    const t = s.title.trim().toLowerCase();
    if (t) titleMap.set(t, [...(titleMap.get(t) || []), p.requested]);
    const d = s.description.trim().toLowerCase();
    if (d.length > 30) descMap.set(d, [...(descMap.get(d) || []), p.requested]);
  }

  for (const urls of titleMap.values()) if (urls.length > 1) add("", issue("medium", "site", "Duplicate title tags across pages", "Make titles unique per intent.", urls));
  for (const urls of descMap.values()) if (urls.length > 1) add("", issue("low", "site", "Duplicate meta descriptions", "Make descriptions unique for key pages.", urls));

  for (const p of pages) {
    if (!(p.status === 200 && p.isHtml && p.signals)) continue;
    for (const t of p.signals.internalLinks) {
      const tp = byReq.get(t) || byFinal.get(norm(t, options.includeQuery));
      if (!tp) continue;
      if (tp.error || tp.status >= 400) add(p.requested, issue("high", "link", `Broken internal link: ${t}`, "Fix or remove broken internal links.", [p.requested, t]));
      else if ((tp.redirects || []).length > 1) add(p.requested, issue("low", "link", `Internal link redirects: ${t}`, "Point links directly to final URLs.", [p.requested, tp.final]));
    }
  }

  if (!indexable) add("", issue(devHost ? "info" : "critical", "site", "No indexable HTML pages detected", devHost ? "Expected in dev if noindex is intentional. Remove for production." : "Remove indexability blockers before launch."));

  const counts = { critical: 0, high: 0, medium: 0, low: 0, info: 0 };
  for (const i of issues) counts[i.severity] += 1;
  const score = Math.max(0, Math.round(100 - counts.critical * 18 - counts.high * 8 - counts.medium * 3 - counts.low));
  return { issues, perPage, counts, score, html200, indexable, noindex };
}

function sortIssues(list) {
  return [...list].sort((a, b) => {
    const d = WEIGHT[a.severity] - WEIGHT[b.severity];
    if (d) return d;
    return a.summary.localeCompare(b.summary);
  });
}

function short(u) {
  try {
    const x = new URL(u);
    return `${x.pathname || "/"}${x.search}`;
  } catch {
    return u;
  }
}

function esc(s) {
  return String(s || "").replace(/\|/g, "\\|").replace(/\n/g, " ");
}

function md(ctx) {
  const { options, target, crawlData, robotsRes, sitemapRes, analysis, now } = ctx;
  const issues = sortIssues(analysis.issues);
  const top = issues.slice(0, 20);
  const lines = [];
  lines.push("# SEO Audit Report", "");
  lines.push(`- Generated: ${now.toISOString()}`);
  lines.push(`- Target: ${target}`);
  lines.push(`- Crawl budget: ${options.maxPages}`, "");
  lines.push("## Executive Summary", "");
  lines.push(`- Score: **${analysis.score}/100**`);
  lines.push(`- Pages crawled: **${crawlData.pages.length}**`);
  lines.push(`- HTML 200 pages: **${analysis.html200}**`);
  lines.push(`- Estimated indexable pages: **${analysis.indexable}**`);
  lines.push(`- noindex pages: **${analysis.noindex}**`);
  lines.push(`- Issues: **${analysis.counts.critical} critical**, **${analysis.counts.high} high**, **${analysis.counts.medium} medium**, **${analysis.counts.low} low**, **${analysis.counts.info} info**`, "");
  lines.push("## Priority Fixes", "");
  if (!top.length) lines.push("- No issues found.");
  else for (const i of top) lines.push(`- [${i.severity.toUpperCase()}] ${i.summary} - ${i.fix}`);
  lines.push("", "## Crawl Signals", "");
  lines.push(`- robots.txt status: ${robotsRes.status || "error"}${robotsRes.error ? ` (${robotsRes.error})` : ""}`);
  lines.push(`- Sitemap URLs discovered: ${sitemapRes.urls.length}`);
  lines.push(`- Sitemap sample failures: ${sitemapRes.sample.filter((x) => x.status >= 400 || x.status === 0).length}`);
  lines.push(`- Crawl truncated: ${crawlData.truncated ? "yes" : "no"}`, "");
  lines.push("## Page Snapshot", "");
  lines.push("| URL | Status | Indexable | Title | Description | Canonical | H1 | Issues |");
  lines.push("| --- | ---: | :---: | --- | --- | --- | ---: | ---: |");
  for (const p of crawlData.pages) {
    const s = p.signals;
    const idx = p.status === 200 && s && !/\bnoindex\b/i.test(`${s.robots} ${p.headers["x-robots-tag"] || ""}`) ? "yes" : "no";
    lines.push(`| ${esc(short(p.requested))} | ${p.status || "err"} | ${idx} | ${esc((s?.title || "").slice(0, 60))} | ${esc((s?.description || "").slice(0, 60))} | ${esc(short(s?.canonical || ""))} | ${s?.h1 ?? 0} | ${(analysis.perPage.get(p.requested) || []).length} |`);
  }
  lines.push("", "## Issues (All)", "");
  lines.push("| Severity | Scope | Summary | Fix | Details | URLs |");
  lines.push("| --- | --- | --- | --- | --- | --- |");
  for (const i of issues) lines.push(`| ${i.severity} | ${i.scope} | ${esc(i.summary)} | ${esc(i.fix)} | ${esc(i.details || "")} | ${esc((i.urls || []).slice(0, 5).map(short).join(", "))} |`);
  lines.push("", "## Per-Page Findings", "");
  for (const p of crawlData.pages) {
    const f = sortIssues(analysis.perPage.get(p.requested) || []);
    if (!f.length) continue;
    lines.push(`### ${short(p.requested)}`, "");
    lines.push(`- Status: ${p.status || "error"}${p.error ? ` (${p.error})` : ""}`);
    lines.push(`- Final URL: ${p.final}`);
    lines.push(`- Response time: ${p.ms} ms`);
    if (p.signals) {
      lines.push(`- Title: ${p.signals.title || "(missing)"}`);
      lines.push(`- Meta description length: ${p.signals.description.length}`);
      lines.push(`- Headings: h1=${p.signals.h1}, h2=${p.signals.h2}, words=${p.signals.words}`);
      lines.push(`- Images: ${p.signals.images} (missing alt: ${p.signals.missingAlt})`);
    }
    lines.push("- Findings:");
    for (const x of f) lines.push(`  - [${x.severity.toUpperCase()}] ${x.summary} - ${x.fix}`);
    lines.push("");
  }
  lines.push("## Sitemap Sample", "");
  lines.push("| URL | Status | Final URL | Error |");
  lines.push("| --- | ---: | --- | --- |");
  for (const s of sitemapRes.sample) lines.push(`| ${esc(short(s.url))} | ${s.status || "err"} | ${esc(short(s.final || ""))} | ${esc(s.error || "")} |`);
  lines.push("", "## Notes", "", "- Automated report; still do manual review for content quality and keyword targeting.", "- Re-run after fixes and compare issue counts and score trend.", "");
  return lines.join("\n");
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const target = siteUrl(options.site);
  options.site = target.toString();
  process.stderr.write(`[seo-audit] Target ${options.site}\n`);

  const robotsRes = await fetchUrl(`${target.origin}/robots.txt`, options, true);
  const robots = robotsRes.status === 200 ? parseRobots(robotsRes.body) : { rules: [], sitemaps: [] };

  const smQueue = new Set((robots.sitemaps.length ? robots.sitemaps : [`${target.origin}/sitemap.xml`]).map((x) => {
    try { return new URL(x, target.origin).toString(); } catch { return ""; }
  }).filter(Boolean));
  const sitemapUrls = new Set();
  for (const sm of smQueue) {
    const r = await fetchUrl(sm, options, true);
    if (r.status !== 200 || !r.body) continue;
    for (const m of r.body.matchAll(/<loc>([\s\S]*?)<\/loc>/gi)) {
      const loc = decode(strip(m[1])).trim();
      if (!loc) continue;
      try {
        const u = new URL(loc, sm).toString();
        if (/sitemap/i.test(u) && /\.xml(?:$|\?)/i.test(u)) smQueue.add(u);
        else sitemapUrls.add(u);
      } catch {}
    }
    if (sitemapUrls.size > 5000) break;
  }
  const sample = [...sitemapUrls].slice(0, options.sitemapSample);
  const sampleChecks = [];
  for (const u of sample) {
    const r = await fetchUrl(u, options, false);
    sampleChecks.push({ url: u, status: r.status, final: r.final, error: r.error });
  }
  const sitemapRes = { urls: [...sitemapUrls], sample: sampleChecks };

  const crawlData = await crawl(target.toString(), options, robots.rules || []);
  const analysis = analyze(crawlData, robotsRes, sitemapRes, options);
  const report = md({ options, target: target.toString(), crawlData, robotsRes, sitemapRes, analysis, now: new Date() });
  const out = resolve(options.out);
  await mkdir(dirname(out), { recursive: true });
  await writeFile(out, report, "utf8");

  process.stdout.write(`SEO report written to: ${out}\n`);
  process.stdout.write(`Score ${analysis.score}/100 | issues critical=${analysis.counts.critical} high=${analysis.counts.high} medium=${analysis.counts.medium} low=${analysis.counts.low} info=${analysis.counts.info}\n`);
}

main().catch((e) => {
  process.stderr.write(`seo-audit failed: ${e instanceof Error ? e.message : String(e)}\n`);
  process.exit(1);
});

