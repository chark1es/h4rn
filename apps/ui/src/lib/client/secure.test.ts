import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { kindForEndpoint, normalizeSecureUrl } from "./secure";

const here = dirname(fileURLToPath(import.meta.url));
/** apps/ui/src/lib/client -> repository root. */
const repo = resolve(here, "../../../../..");
const read = (relative: string): string => readFileSync(resolve(repo, relative), "utf8");

describe("candidate URLs", () => {
  it("turns a bare LAN or mDNS host into a cleartext secure endpoint", () => {
    expect(normalizeSecureUrl("192.168.1.20:47800")).toBe("ws://192.168.1.20:47800/secure");
    expect(normalizeSecureUrl("daemon.local:47800")).toBe("ws://daemon.local:47800/secure");
    expect(normalizeSecureUrl("100.101.102.103:47800")).toBe("ws://100.101.102.103:47800/secure");
  });

  it("keeps an explicit ws(s) candidate and maps http(s) onto it", () => {
    expect(normalizeSecureUrl("ws://192.168.1.20:47800/secure")).toBe(
      "ws://192.168.1.20:47800/secure",
    );
    expect(normalizeSecureUrl("http://daemon.local:47800")).toBe("ws://daemon.local:47800/secure");
    expect(normalizeSecureUrl("https://relay.example.com/relay")).toBe(
      "wss://relay.example.com/relay",
    );
  });

  it("ranks CGNAT and MagicDNS candidates as tailscale", () => {
    expect(kindForEndpoint("ws://100.64.0.1:47800/secure")).toBe("tailscale");
    expect(kindForEndpoint("ws://100.127.255.254:47800/secure")).toBe("tailscale");
    expect(kindForEndpoint("wss://host.tail1234.ts.net/secure")).toBe("tailscale");
    expect(kindForEndpoint("ws://192.168.1.20:47800/secure")).toBe("direct");
    expect(kindForEndpoint("ws://daemon.local:47800/secure")).toBe("direct");
  });
});

describe("mobile cleartext policy", () => {
  it("lets a release Android build reach private, CGNAT and mDNS candidates", () => {
    const xml = read("apps/mobile/native/android/app/src/main/res/xml/network_security_config.xml");
    expect(xml).toMatch(/<base-config cleartextTrafficPermitted="true"/);

    const gradle = read("apps/mobile/native/android/app/build.gradle.kts");
    expect(gradle).toMatch(/manifestPlaceholders\["usesCleartextTraffic"\] = "true"/);
  });

  it("keeps iOS ATS to DNS-name exceptions instead of inert CIDR keys", () => {
    const plist = read("apps/mobile/src-tauri/Info.plist");
    expect(plist).toContain("<key>NSAllowsLocalNetworking</key>");
    for (const host of ["local", "ts.net"]) {
      expect(plist).toContain(`<key>${host}</key>`);
    }
    // ATS matches DNS names, not address ranges, so a CIDR key is inert and
    // must not creep back in as if it excepted private/CGNAT IP literals.
    for (const cidr of [
      "10.0.0.0/8",
      "172.16.0.0/12",
      "192.168.0.0/16",
      "100.64.0.0/10",
    ]) {
      expect(plist).not.toContain(`<key>${cidr}</key>`);
    }

    const tauri = JSON.parse(read("apps/mobile/src-tauri/tauri.conf.json")) as {
      bundle: { iOS: { infoPlist?: string } };
    };
    expect(tauri.bundle.iOS.infoPlist).toBe("Info.plist");
  });
});
