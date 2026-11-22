/**
 * HTML preview preparation.
 *
 * The preview never loads a page from the daemon as a URL. The HTML is read as
 * text, everything it needs from the workspace is inlined, and the result is
 * rendered in a sandboxed iframe. In a packaged shell the prepared document is
 * served from the shell's own `h4rn-preview` origin (so the page's scripts are
 * not blocked by the window's `script-src 'self'`); in the browser build it is
 * rendered through `srcdoc`. Either way:
 *
 * - the page has an opaque origin, so it cannot read the app, its cookies or
 *   its storage;
 * - relative references are resolved here, against the document's own folder,
 *   and only paths inside the workspace root are fetched;
 * - a strict CSP (prepended below) blocks anything that was not inlined except
 *   images, styles, scripts and fonts from `https:` CDNs.
 *
 * The functions in this file are deliberately free of DOM APIs so they can be
 * unit-tested in the node test environment.
 */

/** Sandbox the iframe gets. Scripts run; nothing else is granted. */
export const PREVIEW_SANDBOX = "allow-scripts";

/** The page's policy. `default-src 'none'` is the floor everything else adds to. */
export const PREVIEW_CSP =
  "default-src 'none'; img-src data: https:; media-src data: https:; object-src data:; style-src 'unsafe-inline' https:; script-src 'unsafe-inline' https:; font-src data: https:; connect-src https:";

/** The CSP `<meta>` the document starts with. */
export const PREVIEW_CSP_META = `<meta http-equiv="Content-Security-Policy" content="${PREVIEW_CSP}">`;

/** Total workspace bytes the preview will inline, including the HTML itself. */
export const PREVIEW_MAX_TOTAL_BYTES = 10 * 1024 * 1024;

/**
 * The largest text read the preview asks the daemon for (400 KiB).
 *
 * The secure transport refuses a frame over 512 KiB, and the JSON envelope
 * around a decoded document or stylesheet needs headroom. The daemon is asked
 * for this bound rather than reading a larger file and failing to send it.
 * Escaping can still push a file under this bound past one frame, so the
 * daemon measures the serialized response and answers `too_large` instead of
 * sending a frame the transport would refuse.
 */
export const PREVIEW_MAX_TEXT_BYTES = 400 * 1024;

/** The largest HTML document the preview will read, bounded by one frame. */
export const PREVIEW_MAX_DOCUMENT_BYTES = PREVIEW_MAX_TEXT_BYTES;

/**
 * The largest asset the daemon embeds as base64 in one response (360 KiB).
 *
 * Keep in step with `MAX_INLINE_BYTES` in
 * `crates/h4rn-daemon/src/fs/paths.rs`. Base64 makes that 480 KiB, which still
 * fits the secure transport's 512 KiB frame once the JSON envelope is added.
 */
export const PREVIEW_MAX_ASSET_BYTES = 360 * 1024;

/** What one workspace reference needs from the daemon. */
export type PreviewAssetKind = "text" | "bytes";

/** One reference found in the document, already resolved to a workspace path. */
export type PreviewReference = {
  /** The raw reference as it appears in the document. */
  url: string;
  /** Workspace-relative path. */
  path: string;
  /** Text assets (CSS, JS) are inlined as text; everything else as bytes. */
  kind: PreviewAssetKind;
  /** How many times this document embeds the asset. */
  count: number;
};

/** The result of scanning a document for workspace references. */
export type PreviewScan = {
  refs: PreviewReference[];
  /** Absolute `https:` (and protocol-relative) references left to the network. */
  external: string[];
  /** References that resolve outside the workspace or use another scheme. */
  rejected: string[];
};

/** One fetched asset, keyed by workspace path. */
export type PreviewAsset =
  | { kind: "text"; text: string }
  | { kind: "bytes"; base64: string; mime: string };

export type PreviewAssetMap = Record<string, PreviewAsset>;

export type PreviewDocument = {
  srcdoc: string;
  /** The token the frame's bridge uses when it reports a link. */
  token: string;
  /** Paths that were inlined into the document. */
  inlined: string[];
  /** References whose bytes were not available and are therefore not loaded. */
  missing: string[];
};

/** Whether one more asset fits in the preview's total byte budget. */
export function previewAssetFits(
  totalBytes: number,
  nextBytes: number,
  cap = PREVIEW_MAX_TOTAL_BYTES,
): boolean {
  return totalBytes + nextBytes <= cap;
}

/** True for the two extensions the Preview tab accepts. */
export function isHtmlPath(path: string): boolean {
  const clean = path.trim().split(/[?#]/, 1)[0] ?? "";
  return /\.html?$/i.test(clean);
}

/**
 * Resolves a reference from a document or stylesheet to a workspace path.
 *
 * Returns `null` for anything that is not a same-workspace relative asset:
 * absolute URLs, protocol-relative URLs, other schemes, fragment-only links
 * and paths that climb above the workspace root. A leading `/` is treated as
 * the workspace root, the way a static server would serve it.
 */
export function resolvePreviewAsset(docPath: string, raw: string): string | null {
  const url = raw.trim();
  if (!url || url.startsWith("#")) return null;
  if (url.startsWith("//")) return null;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(url)) return null;
  if (url.includes("\0")) return null;

  const withoutQuery = url.split(/[?#]/, 1)[0] ?? "";
  let decoded = withoutQuery;
  try {
    decoded = decodeURIComponent(withoutQuery);
  } catch {
    // A malformed escape is not a path we can trust.
    return null;
  }
  const normalized = decoded.replace(/\\/g, "/");
  const segments = normalized.startsWith("/") ? [] : docDirectory(docPath);
  for (const segment of normalized.split("/")) {
    if (!segment || segment === ".") continue;
    if (segment === "..") {
      if (segments.length === 0) return null;
      segments.pop();
      continue;
    }
    segments.push(segment);
  }
  return segments.length > 0 ? segments.join("/") : null;
}

/** The folder of a workspace path (`reports/chart.html` -> `["reports"]`). */
function docDirectory(docPath: string): string[] {
  const parts = docPath.replace(/\\/g, "/").split("/");
  parts.pop();
  return parts.filter(Boolean);
}

/** Whether a reference is an absolute URL the sandboxed page may load itself. */
export function isExternalPreviewUrl(raw: string): boolean {
  const url = raw.trim();
  return /^https?:/i.test(url) || url.startsWith("//");
}

/**
 * Validates a link the page asked to open and names its host.
 *
 * The page can send anything — including a forged token, which the panel
 * checks separately — so the URL is re-validated here and only `http:` and
 * `https:` ever reach the system browser.
 */
export function previewOpenRequest(raw: unknown): { url: string; host: string } | null {
  if (typeof raw !== "string") return null;
  const url = raw.trim();
  if (!/^https?:\/\//i.test(url)) return null;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
  if (!parsed.hostname) return null;
  return { url: parsed.href, host: parsed.host };
}

/**
 * True when a click on a markdown `.html` link should open the preview.
 *
 * Modifier-clicks keep the browser's own behaviour (a new tab, a download),
 * so a ⌘-click never hijacks the link.
 */
export function previewLinkOpens(event: {
  metaKey?: boolean;
  ctrlKey?: boolean;
  shiftKey?: boolean;
  altKey?: boolean;
  button?: number;
}): boolean {
  if (event.button !== undefined && event.button !== 0) return false;
  return !(event.metaKey || event.ctrlKey || event.shiftKey || event.altKey);
}

/** How many bytes one asset adds to the serialized document. */
export function previewEmbeddedBytes(asset: PreviewAsset): number {
  const text = asset.kind === "text" ? asset.text : asset.base64;
  return new TextEncoder().encode(text).length;
}

/** One reference before occurrence counting: what the document asked for. */
type PreviewTarget = Omit<PreviewReference, "count">;

/** Classifies one reference and resolves the workspace ones. */
function classify(
  docPath: string,
  raw: string,
  kind: PreviewAssetKind,
  scan: PreviewScan,
): PreviewTarget | null {
  const url = raw.trim();
  if (!url || url.startsWith("#") || /^data:/i.test(url) || /^blob:/i.test(url)) return null;
  if (isExternalPreviewUrl(url)) {
    scan.external.push(url);
    return null;
  }
  const path = resolvePreviewAsset(docPath, url);
  if (!path) {
    if (!url.startsWith("#")) scan.rejected.push(url);
    return null;
  }
  return { url, path, kind };
}

type Attr = { name: string; value: string | null };

type Tag = {
  name: string;
  attrs: Attr[];
  /** The exact source text, used when the tag is left alone. */
  raw: string;
  /** Offset just past the closing `>`. */
  end: number;
  closing: boolean;
  selfClosing: boolean;
};

const RAW_TEXT_ELEMENTS = new Set(["script", "style", "textarea", "title"]);

/** Reads one tag starting at `start` (which must point at `<`). */
function readTag(source: string, start: number): Tag | null {
  let index = start + 1;
  let closing = false;
  if (source[index] === "/") {
    closing = true;
    index += 1;
  }
  const nameMatch = /^[a-zA-Z][a-zA-Z0-9:_-]*/.exec(source.slice(index));
  if (!nameMatch) return null;
  const name = nameMatch[0];
  index += name.length;

  const attrs: Attr[] = [];
  let selfClosing = false;
  while (index < source.length) {
    while (index < source.length && /\s/.test(source[index] ?? "")) index += 1;
    if (source.startsWith("/>", index)) {
      selfClosing = true;
      index += 2;
      return { name, attrs, raw: source.slice(start, index), end: index, closing, selfClosing };
    }
    if (source[index] === ">") {
      index += 1;
      return { name, attrs, raw: source.slice(start, index), end: index, closing, selfClosing };
    }
    const attrName = /^[^\s=/>]+/.exec(source.slice(index));
    if (!attrName) {
      // A stray character; consume it so the parser always advances.
      index += 1;
      continue;
    }
    const attr: Attr = { name: attrName[0], value: null };
    index += attrName[0].length;
    let lookahead = index;
    while (lookahead < source.length && /\s/.test(source[lookahead] ?? "")) lookahead += 1;
    if (source[lookahead] === "=") {
      lookahead += 1;
      while (lookahead < source.length && /\s/.test(source[lookahead] ?? "")) lookahead += 1;
      const quote = source[lookahead];
      if (quote === '"' || quote === "'") {
        const close = source.indexOf(quote, lookahead + 1);
        if (close < 0) return null;
        attr.value = source.slice(lookahead + 1, close);
        index = close + 1;
      } else {
        const value = /^[^\s>]+/.exec(source.slice(lookahead));
        if (!value) return null;
        attr.value = value[0];
        index = lookahead + value[0].length;
      }
    }
    attrs.push(attr);
  }
  return null;
}

function attr(tag: Tag, name: string): string | null {
  return tag.attrs.find((entry) => entry.name.toLowerCase() === name)?.value ?? null;
}

function escapeAttr(value: string): string {
  return value.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}

function formatTag(tag: Tag, attrs = tag.attrs): string {
  const body = attrs
    .map((entry) =>
      entry.value === null ? ` ${entry.name}` : ` ${entry.name}="${escapeAttr(entry.value)}"`,
    )
    .join("");
  return `<${tag.closing ? "/" : ""}${tag.name}${body}${tag.selfClosing ? "/" : ""}>`;
}

function upsertAttr(attrs: Attr[], name: string, value: string): Attr[] {
  return [
    ...attrs.filter((entry) => entry.name.toLowerCase() !== name.toLowerCase()),
    { name, value },
  ];
}

function dropAttrs(attrs: Attr[], names: string[]): Attr[] {
  const lowered = new Set(names.map((name) => name.toLowerCase()));
  return attrs.filter((entry) => !lowered.has(entry.name.toLowerCase()));
}

/** One `srcset` candidate: a URL and its width/density descriptor. */
export type SrcsetCandidate = { url: string; descriptor: string };

/**
 * Splits a `srcset` value into its candidates.
 *
 * A `data:` candidate is left alone by the caller: its commas are part of the
 * URL, not separators, and it never needs the workspace.
 */
export function parseSrcset(value: string): SrcsetCandidate[] {
  const candidates: SrcsetCandidate[] = [];
  for (const part of value.split(",")) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const match = /^(\S+)(?:\s+([\s\S]*))?$/.exec(trimmed);
    if (!match) continue;
    candidates.push({ url: match[1] ?? "", descriptor: (match[2] ?? "").trim() });
  }
  return candidates;
}

/** True when a `srcset` carries a `data:` URL whose commas cannot be split. */
function srcsetHasDataUrl(value: string): boolean {
  return /(^|,)\s*data:/i.test(value);
}

/** Finds the closing tag for a raw-text element, starting at `from`. */
function findClosing(source: string, name: string, from: number): number {
  const pattern = new RegExp(`</${name}\\s*>`, "i");
  const match = pattern.exec(source.slice(from));
  return match ? from + match.index : -1;
}

/**
 * Walks tags while leaving comments and raw-text element bodies alone.
 *
 * The callback returns a replacement for the opening tag, or `null` to keep it.
 * Raw-text bodies (script, style, textarea, title) are passed to `rawBody`
 * when the element is kept, so stylesheet `url()`s can still be rewritten.
 */
function walk(
  html: string,
  visit: (tag: Tag) => { replacement: string | null; skipBody?: boolean } | null,
  rawBody?: (name: string, body: string) => string,
): string {
  let out = "";
  let index = 0;
  while (index < html.length) {
    const open = html.indexOf("<", index);
    if (open < 0) {
      out += html.slice(index);
      break;
    }
    out += html.slice(index, open);
    if (html.startsWith("<!--", open)) {
      const close = html.indexOf("-->", open + 4);
      const end = close < 0 ? html.length : close + 3;
      out += html.slice(open, end);
      index = end;
      continue;
    }
    if (html.startsWith("<!", open) || html.startsWith("<?", open)) {
      const close = html.indexOf(">", open + 2);
      const end = close < 0 ? html.length : close + 1;
      out += html.slice(open, end);
      index = end;
      continue;
    }
    const tag = readTag(html, open);
    if (!tag) {
      out += "<";
      index = open + 1;
      continue;
    }
    const decision = visit(tag);
    out += decision?.replacement ?? tag.raw;
    index = tag.end;

    const lower = tag.name.toLowerCase();
    if (!tag.closing && !tag.selfClosing && RAW_TEXT_ELEMENTS.has(lower)) {
      const close = findClosing(html, tag.name, index);
      if (close < 0) {
        if (!decision?.skipBody) out += html.slice(index);
        index = html.length;
        continue;
      }
      if (decision?.skipBody) {
        // The replacement swallowed the element, including its closing tag.
        const closeTag = readTag(html, close);
        index = closeTag ? closeTag.end : close + lower.length + 2;
        continue;
      }
      const body = html.slice(index, close);
      out += rawBody?.(lower, body) ?? body;
      index = close;
    }
  }
  return out;
}

/**
 * Finds every workspace asset a document references.
 *
 * Stylesheets and scripts are text; images and other media are bytes. The
 * scan skips comments and script/style bodies, so a script string that looks
 * like an `<img>` tag is not mistaken for markup.
 */
export function scanPreviewReferences(html: string, docPath: string): PreviewScan {
  const scan: PreviewScan = { refs: [], external: [], rejected: [] };
  const seen = new Map<string, PreviewReference>();
  const add = (ref: PreviewTarget | null) => {
    if (!ref) return;
    const key = `${ref.kind}:${ref.path}`;
    const existing = seen.get(key);
    if (existing) {
      existing.count += 1;
      return;
    }
    const entry = { ...ref, count: 1 };
    seen.set(key, entry);
    scan.refs.push(entry);
  };

  walk(
    html,
    (tag) => {
      if (tag.closing) return null;
      const name = tag.name.toLowerCase();
      const inline = attr(tag, "style");
      if (inline) {
        for (const ref of scanCssReferences(inline, docPath).refs) add(ref);
      }
      if (name === "link") {
        const rel = attr(tag, "rel")?.toLowerCase() ?? "";
        const href = attr(tag, "href");
        if (href && rel.split(/\s+/).includes("stylesheet")) {
          add(classify(docPath, href, "text", scan));
        }
        return null;
      }
      if (name === "script") {
        const src = attr(tag, "src");
        if (src) add(classify(docPath, src, "text", scan));
        return null;
      }
      if (name === "img" || name === "source") {
        const src = attr(tag, "src");
        if (src) add(classify(docPath, src, "bytes", scan));
        const srcset = attr(tag, "srcset");
        if (srcset) {
          for (const candidate of parseSrcset(srcset)) {
            add(classify(docPath, candidate.url, "bytes", scan));
          }
        }
        return null;
      }
      if (name === "video" || name === "audio" || name === "track" || name === "iframe") {
        const src = attr(tag, "src");
        if (src) add(classify(docPath, src, "bytes", scan));
        if (name === "video") {
          const poster = attr(tag, "poster");
          if (poster) add(classify(docPath, poster, "bytes", scan));
        }
        return null;
      }
      if (name === "object") {
        const data = attr(tag, "data");
        if (data) add(classify(docPath, data, "bytes", scan));
        return null;
      }
      if (name === "embed") {
        const src = attr(tag, "src");
        if (src) add(classify(docPath, src, "bytes", scan));
        return null;
      }
      if (name === "svg" || name === "image" || name === "use") {
        for (const value of [attr(tag, "href"), attr(tag, "xlink:href")]) {
          if (value) add(classify(docPath, value, "bytes", scan));
        }
        return null;
      }
      return null;
    },
    (name, body) => {
      if (name === "style") {
        for (const ref of scanCssReferences(body, docPath).refs) add(ref);
      }
      return body;
    },
  );
  return scan;
}

/** Matches one `@import` at-rule, including its optional media condition. */
const AT_IMPORT =
  /@import\s+(?:url\(\s*)?(?:"([^"]*)"|'([^']*)'|([^)'"\s;]+))\s*\)?\s*([^;]*)(?:;|$)/gi;

/**
 * Finds `url(...)` assets and `@import`ed stylesheets in a stylesheet.
 *
 * Imports are text: the preview inlines the imported sheet's CSS rather than
 * turning it into a `data:` URL. The import rules are removed before the
 * `url()` scan so the same reference is not counted twice.
 */
export function scanCssReferences(css: string, cssPath: string): PreviewScan {
  const scan: PreviewScan = { refs: [], external: [], rejected: [] };
  const seen = new Map<string, PreviewReference>();
  const add = (ref: PreviewTarget | null) => {
    if (!ref) return;
    const key = `${ref.kind}:${ref.path}`;
    const existing = seen.get(key);
    if (existing) {
      existing.count += 1;
      return;
    }
    const entry = { ...ref, count: 1 };
    seen.set(key, entry);
    scan.refs.push(entry);
  };

  const stripped = css.replace(
    AT_IMPORT,
    (_whole, doubleQuoted: string, singleQuoted: string, bare: string) => {
      add(classify(cssPath, doubleQuoted ?? singleQuoted ?? bare ?? "", "text", scan));
      return " ";
    },
  );

  for (const match of stripped.matchAll(/url\(\s*(?:"([^"]*)"|'([^']*)'|([^)'"\s]+))\s*\)/gi)) {
    const url = match[1] ?? match[2] ?? match[3] ?? "";
    add(classify(cssPath, url, "bytes", scan));
  }
  return scan;
}

/** A `data:` URL for bytes, using the extension for the media type. */
export function previewMimeType(path: string): string {
  const extension = path.split(".").at(-1)?.toLowerCase() ?? "";
  const known: Record<string, string> = {
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    gif: "image/gif",
    webp: "image/webp",
    avif: "image/avif",
    svg: "image/svg+xml",
    ico: "image/x-icon",
    bmp: "image/bmp",
    woff: "font/woff",
    woff2: "font/woff2",
    ttf: "font/ttf",
    otf: "font/otf",
    mp4: "video/mp4",
    m4v: "video/mp4",
    webm: "video/webm",
    ogv: "video/ogg",
    ogg: "audio/ogg",
    mp3: "audio/mpeg",
    wav: "audio/wav",
    m4a: "audio/mp4",
    vtt: "text/vtt",
    srt: "application/x-subrip",
    pdf: "application/pdf",
  };
  return known[extension] ?? "application/octet-stream";
}

function dataUrl(asset: PreviewAsset, path: string): string | null {
  if (asset.kind !== "bytes") return null;
  const mime = asset.mime || previewMimeType(path);
  return `data:${mime};base64,${asset.base64}`;
}

function inlineCss(
  css: string,
  cssPath: string,
  assets: PreviewAssetMap,
  missing?: Set<string>,
  seen: Set<string> = new Set(),
): string {
  if (seen.has(cssPath)) return "";
  seen.add(cssPath);

  // An `@import` of a workspace stylesheet becomes that stylesheet's own CSS,
  // wrapped in its media condition; it is never a `data:` URL or a relative
  // request against the preview origin. External imports are the page's own
  // business and stay as written.
  const imported = css.replace(
    AT_IMPORT,
    (_whole, doubleQuoted: string, singleQuoted: string, bare: string, media: string) => {
      const raw = (doubleQuoted ?? singleQuoted ?? bare ?? "").trim();
      if (!raw || /^(data|blob):/i.test(raw)) return "";
      if (isExternalPreviewUrl(raw) || /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(raw)) return _whole;
      const path = resolvePreviewAsset(cssPath, raw);
      if (!path) {
        missing?.add(raw);
        return "";
      }
      const asset = assets[path];
      if (asset?.kind !== "text") {
        missing?.add(path);
        return "";
      }
      const nested = inlineCss(asset.text, path, assets, missing, seen);
      const condition = (media ?? "").trim();
      return condition ? `@media ${condition} {${nested}}` : nested;
    },
  );

  const rewritten = imported.replace(
    /url\(\s*(?:"([^"]*)"|'([^']*)'|([^)'"\s]+))\s*\)/gi,
    (whole, doubleQuoted: string, singleQuoted: string, bare: string) => {
      const raw = (doubleQuoted ?? singleQuoted ?? bare ?? "").trim();
      if (!raw || raw.startsWith("#") || /^data:/i.test(raw)) return whole;
      if (isExternalPreviewUrl(raw) || /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(raw)) return whole;
      const path = resolvePreviewAsset(cssPath, raw);
      if (!path) {
        // A local reference that escapes the workspace is not left as a
        // relative URL: the preview origin would see the request.
        missing?.add(raw);
        return "none";
      }
      const asset = assets[path];
      if (!asset) {
        missing?.add(path);
        return "none";
      }
      const replacement = dataUrl(asset, path);
      if (!replacement) {
        missing?.add(path);
        return "none";
      }
      return `url("${replacement}")`;
    },
  );
  // A stylesheet cannot close its own element.
  return rewritten.replace(/<\/style/gi, "<\\/style");
}

/**
 * Injects the policy as the document's first bytes.
 *
 * There is no `<head>` matching: untrusted markup cannot make the policy land
 * anywhere but the implicit head. The one exception is a doctype — a meta
 * before it would switch the page into quirks mode — so leading whitespace and
 * comments are skipped while locating it, and the policy goes directly after it.
 */
export function injectCsp(html: string): string {
  const index = doctypeIndex(html);
  if (index >= 0) {
    const doctype = /^<!doctype\b[^>]*>/i.exec(html.slice(index));
    if (doctype) {
      return `${html.slice(0, index)}${doctype[0]}${PREVIEW_CSP_META}${html.slice(
        index + doctype[0].length,
      )}`;
    }
  }
  return `${PREVIEW_CSP_META}${html}`;
}

/** The offset of a leading doctype, skipping only whitespace and comments. */
function doctypeIndex(html: string): number {
  let index = 0;
  while (index < html.length) {
    while (index < html.length && /\s/.test(html[index] ?? "")) index += 1;
    if (html.startsWith("<!--", index)) {
      const close = html.indexOf("-->", index + 4);
      if (close < 0) return -1;
      index = close + 3;
      continue;
    }
    if (html.startsWith("<?", index)) {
      const close = html.indexOf(">", index + 2);
      if (close < 0) return -1;
      index = close + 1;
      continue;
    }
    break;
  }
  return /^<!doctype\b/i.test(html.slice(index)) ? index : -1;
}

/**
 * Makes links inert in the frame and reports them to the app.
 *
 * The sandbox has no `allow-popups`, so `target="_blank"` alone does nothing;
 * the bridge script intercepts the click and posts the URL to the parent.
 * The panel validates it and asks the user before opening it, because the
 * page controls everything inside the frame, including this token.
 * Same-page `#` links stay in-page.
 */
export function previewBridge(token: string): string {
  const safe = token.replace(/[^a-zA-Z0-9-]/g, "");
  return `<script>(function(){var token=${JSON.stringify(safe)};document.addEventListener("click",function(event){var node=event.target;while(node&&node.tagName!=="A"){node=node.parentNode}if(!node)return;var href=node.getAttribute("href")||"";if(!href||href.charAt(0)==="#")return;event.preventDefault();parent.postMessage({type:"h4rn:preview-open",token:token,href:href},"*")},true);document.addEventListener("submit",function(event){event.preventDefault()},true)})();</script>`;
}

function rewriteAnchor(tag: Tag): string {
  const href = attr(tag, "href");
  if (!href) return tag.raw;
  // Same-page links scroll inside the frame; a new target would lose the anchor.
  if (href.trim().startsWith("#")) return tag.raw;
  let attrs = upsertAttr(tag.attrs, "target", "_blank");
  attrs = upsertAttr(attrs, "rel", "noopener noreferrer");
  return formatTag(tag, attrs);
}

/**
 * Builds the document for one preview.
 *
 * Inlines the assets in `assets`, removes every local reference it could not
 * inline (so the preview origin never receives a relative request), rewrites
 * anchors for the bridge script, prepends the CSP and appends the bridge.
 */
export function buildPreviewDocument(options: {
  html: string;
  docPath: string;
  assets: PreviewAssetMap;
  token: string;
}): PreviewDocument {
  const { html, docPath, assets, token } = options;
  const inlined = new Set<string>();
  const missing = new Set<string>();

  // `leave` keeps the reference as written (external, `data:`, a fragment);
  // `local` means the document was asking the workspace for the file.
  const classifyLocal = (
    raw: string,
  ): { state: "leave" } | { state: "external" } | { state: "local"; path: string | null } => {
    const value = raw.trim();
    if (!value || value.startsWith("#") || /^(data|blob|javascript|mailto|tel):/i.test(value)) {
      return { state: "leave" };
    }
    if (isExternalPreviewUrl(value)) return { state: "external" };
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(value)) return { state: "leave" };
    return { state: "local", path: resolvePreviewAsset(docPath, value) };
  };

  const take = (raw: string): { path: string; asset: PreviewAsset } | null => {
    const target = classifyLocal(raw);
    if (target.state !== "local") return null;
    if (!target.path) {
      missing.add(raw.trim());
      return null;
    }
    const asset = assets[target.path];
    if (!asset) {
      missing.add(target.path);
      return null;
    }
    inlined.add(target.path);
    return { path: target.path, asset };
  };

  const isLocal = (raw: string): boolean => classifyLocal(raw).state === "local";

  /** A `data:` URL for a local reference, or `null` when it cannot be inlined. */
  const inlineBytes = (raw: string, allowUnknownType = true): string | null => {
    const found = take(raw);
    if (!found) return null;
    if (!allowUnknownType && previewMimeType(found.path) === "application/octet-stream") {
      missing.add(found.path);
      return null;
    }
    const replacement = dataUrl(found.asset, found.path);
    if (!replacement) missing.add(found.path);
    return replacement;
  };

  /** Rewrites one `srcset`; `null` means the attribute must go. */
  const inlineSrcset = (value: string): string | null | undefined => {
    if (srcsetHasDataUrl(value)) return undefined;
    const kept: string[] = [];
    let changed = false;
    for (const candidate of parseSrcset(value)) {
      if (!isLocal(candidate.url)) {
        kept.push(
          candidate.descriptor ? `${candidate.url} ${candidate.descriptor}` : candidate.url,
        );
        continue;
      }
      changed = true;
      const replacement = inlineBytes(candidate.url);
      if (!replacement) continue;
      kept.push(candidate.descriptor ? `${replacement} ${candidate.descriptor}` : replacement);
    }
    return changed ? (kept.length > 0 ? kept.join(", ") : null) : undefined;
  };

  /** Inlines a local `src`/`data`/`href`, or drops the attribute. */
  const inlineAttribute = (
    tag: Tag,
    attrs: Attr[],
    name: string,
    options: { allowUnknownType?: boolean } = {},
  ): { attrs: Attr[]; changed: boolean } => {
    const value = attr(tag, name);
    if (!value || !isLocal(value)) return { attrs, changed: false };
    const replacement = inlineBytes(value, options.allowUnknownType ?? false);
    return {
      attrs: replacement ? upsertAttr(attrs, name, replacement) : dropAttrs(attrs, [name]),
      changed: true,
    };
  };

  const body = walk(
    html,
    (incoming) => {
      if (incoming.closing) return null;
      const styleValue = attr(incoming, "style");
      const tag = styleValue
        ? {
            ...incoming,
            attrs: upsertAttr(
              incoming.attrs,
              "style",
              inlineCss(styleValue, docPath, assets, missing),
            ),
          }
        : incoming;
      const name = tag.name.toLowerCase();

      if (name === "link") {
        const href = attr(tag, "href");
        if (!href) return null;
        const rel = attr(tag, "rel")?.toLowerCase() ?? "";
        if (!rel.split(/\s+/).includes("stylesheet")) {
          // Icons, preloads and manifests are not inlined; a local one must
          // not turn into a request against the preview origin.
          return isLocal(href) ? { replacement: "" } : null;
        }
        const found = take(href);
        if (!found) {
          // External stylesheets stay; a local one that could not be read is
          // dropped rather than left as a relative URL.
          return isLocal(href) ? { replacement: "" } : null;
        }
        if (found.asset.kind !== "text") {
          return { replacement: "" };
        }
        return {
          replacement: `<style>${inlineCss(found.asset.text, found.path, assets, missing)}</style>`,
        };
      }

      if (name === "script") {
        const src = attr(tag, "src");
        if (!src) return null;
        const found = take(src);
        if (!found) {
          return isLocal(src) ? { replacement: "", skipBody: true } : null;
        }
        if (found.asset.kind !== "text") {
          return { replacement: "", skipBody: true };
        }
        // JSON-LD, import maps, templates and modules all keep their type.
        const type = attr(tag, "type");
        const typeAttr = type ? ` type="${escapeAttr(type)}"` : "";
        return {
          replacement: `<script${typeAttr}>${found.asset.text.replace(/<\/script/gi, "<\\/script")}</script>`,
          skipBody: true,
        };
      }

      if (name === "img" || name === "source") {
        const src = attr(tag, "src");
        const srcset = attr(tag, "srcset");
        if (!src && !srcset) return null;
        let attrs = tag.attrs;
        if (src && isLocal(src)) {
          const replacement = inlineBytes(src);
          if (!replacement) return { replacement: "" };
          attrs = upsertAttr(attrs, "src", replacement);
        }
        if (srcset) {
          const next = inlineSrcset(srcset);
          if (next === null) attrs = dropAttrs(attrs, ["srcset"]);
          else if (next !== undefined) attrs = upsertAttr(attrs, "srcset", next);
        }
        return { replacement: formatTag(tag, attrs) };
      }

      if (name === "video" || name === "audio" || name === "track" || name === "iframe") {
        let attrs = tag.attrs;
        let changed = false;
        const names = name === "video" ? ["src", "poster"] : ["src"];
        for (const attribute of names) {
          const result = inlineAttribute(tag, attrs, attribute);
          attrs = result.attrs;
          changed = changed || result.changed;
        }
        return changed || styleValue ? { replacement: formatTag(tag, attrs) } : null;
      }

      if (name === "object") {
        const result = inlineAttribute(tag, tag.attrs, "data");
        return result.changed || styleValue ? { replacement: formatTag(tag, result.attrs) } : null;
      }

      if (name === "embed") {
        const result = inlineAttribute(tag, tag.attrs, "src");
        return result.changed || styleValue ? { replacement: formatTag(tag, result.attrs) } : null;
      }

      if (name === "svg" || name === "image" || name === "use") {
        let attrs = tag.attrs;
        let changed = false;
        for (const attribute of ["href", "xlink:href"]) {
          const value = attr(tag, attribute);
          // A fragment names something already in this document.
          if (!value || value.trim().startsWith("#")) continue;
          const target = classifyLocal(value);
          if (target.state !== "local") continue;
          changed = true;
          const replacement = target.path ? inlineBytes(value, false) : null;
          if (!replacement) {
            if (!target.path) missing.add(value.trim());
            attrs = dropAttrs(attrs, [attribute]);
            continue;
          }
          // `use href="icons.svg#gear"` keeps its fragment on the data URL.
          const hash = value.indexOf("#");
          const fragment = hash >= 0 ? value.slice(hash) : "";
          attrs = upsertAttr(attrs, attribute, `${replacement}${fragment}`);
        }
        return changed || styleValue ? { replacement: formatTag(tag, attrs) } : null;
      }

      if (name === "a") return { replacement: rewriteAnchor(tag) };

      // Nothing else changes, but an inline style may now carry a data URL.
      return styleValue ? { replacement: formatTag(tag) } : null;
    },
    (name, text) => (name === "style" ? inlineCss(text, docPath, assets, missing) : text),
  );

  // The policy is the document's first bytes; the bridge is appended after the
  // document so no `</body>` inside a script or comment can split it.
  const document = injectCsp(body) + previewBridge(token);

  return {
    srcdoc: document,
    token: token.replace(/[^a-zA-Z0-9-]/g, ""),
    inlined: [...inlined],
    missing: [...missing],
  };
}

/** A token that ties the frame's messages to the render that created it. */
export function previewToken(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/** Turns an agent's markdown link into a workspace-relative HTML path. */
export function previewPathFromLink(href: string): string | null {
  const trimmed = href.trim();
  if (!trimmed || trimmed.startsWith("#") || trimmed.startsWith("//")) return null;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(trimmed)) return null;
  if (!isHtmlPath(trimmed)) return null;
  const clean = cleanedLinkPath(trimmed);
  // A link is resolved from the workspace root, so it cannot climb out.
  if (!clean || clean === ".." || clean.startsWith("../") || clean.includes("/../")) return null;
  return clean;
}

function cleanedLinkPath(href: string): string {
  const withoutQuery = href.split(/[?#]/, 1)[0] ?? "";
  let decoded = withoutQuery;
  try {
    decoded = decodeURIComponent(withoutQuery);
  } catch {
    decoded = withoutQuery;
  }
  return decoded.replace(/\\/g, "/").replace(/^\.\//, "").replace(/^\/+/, "");
}
