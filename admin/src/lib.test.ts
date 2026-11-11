// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { datetimeLocal, safeEmailDocument } from "./lib";
describe("email preview isolation", () => {
  it("removes executable markup and remote-resource mechanisms", () => {
    const doc = new DOMParser().parseFromString(
      safeEmailDocument(
        `<script>alert(1)</script><img src="https://tracker.test/pixel"><style>@import 'https://tracker.test/style';</style><iframe src="https://tracker.test"></iframe><form action="https://tracker.test"><input name="secret"></form><a href="javascript:alert(1)" onclick="alert(1)">Link</a><div style="background:url(https://tracker.test)">Text</div>`,
      ),
      "text/html",
    );
    expect(doc.querySelector("script,img,iframe,form,input")).toBeNull();
    expect(
      doc.querySelector("[href],[src],[onclick],[style],[action]"),
    ).toBeNull();
    expect(doc.body.textContent).toContain("Text");
    expect(
      doc
        .querySelector('meta[http-equiv="Content-Security-Policy"]')
        ?.getAttribute("content"),
    ).toContain("default-src 'none'");
  });
  it("preserves readable formatting", () => {
    const doc = new DOMParser().parseFromString(
      safeEmailDocument(
        "<p>Hello <strong>operator</strong></p><table><tr><td>Value</td></tr></table>",
      ),
      "text/html",
    );
    expect(doc.querySelector("strong")?.textContent).toBe("operator");
    expect(doc.querySelector("td")?.textContent).toBe("Value");
  });
});
describe("local date fields", () => {
  it("uses browser local time for persisted timestamps", () => {
    const timestamp = "2026-11-11T14:23:45Z";
    const value = datetimeLocal(timestamp);
    const parsed = new Date(value);
    expect(parsed.getTime()).toBe(new Date(timestamp).getTime() - 45000);
  });
});
