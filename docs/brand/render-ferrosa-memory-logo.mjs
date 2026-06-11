import { chromium } from 'playwright';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const brandDir = dirname(fileURLToPath(import.meta.url));
const svgPath = resolve(brandDir, 'ferrosa-memory-discord-logo.svg');
const out1024 = resolve(brandDir, 'ferrosa-memory-discord-logo-1024.png');
const out512 = resolve(brandDir, 'ferrosa-memory-discord-logo-512.png');
const preview = resolve(brandDir, 'ferrosa-memory-discord-logo-preview.png');
const svg = readFileSync(svgPath, 'utf8');

const browser = await chromium.launch();
try {
  for (const [size, out] of [[1024, out1024], [512, out512]]) {
    const page = await browser.newPage({ viewport: { width: size, height: size }, deviceScaleFactor: 1 });
    await page.setContent(`<html><body style="margin:0;background:transparent;width:${size}px;height:${size}px">${svg}</body></html>`);
    await page.locator('svg').screenshot({ path: out, omitBackground: true });
    await page.close();
  }

  const page = await browser.newPage({ viewport: { width: 1400, height: 760 }, deviceScaleFactor: 1 });
  await page.setContent(`
    <html><body style="margin:0;background:#0a0a0f;color:#e8e8ed;font:16px Inter,-apple-system,sans-serif;">
      <div style="padding:52px;display:grid;grid-template-columns:1fr 1fr;gap:42px;align-items:center;">
        <section>
          <h1 style="margin:0 0 10px;font-size:42px;letter-spacing:-.04em;">Ferrosa Memory Discord logo</h1>
          <p style="color:#9fb5d0;line-height:1.55;max-width:560px;">Ferrosa-family periodic frame + orbital system, but shifted to <strong style="color:#7ee7ff">electric blued steel</strong>, an Fm monogram, and memory graph nodes so it is visually distinct from the warm Fe database mark.</p>
          <div style="display:flex;gap:18px;margin-top:30px;align-items:center;">
            <div style="width:96px;height:96px;border-radius:50%;overflow:hidden;box-shadow:0 0 0 1px #1e2d44,0 20px 60px #000a;">${svg}</div>
            <div style="width:64px;height:64px;border-radius:50%;overflow:hidden;box-shadow:0 0 0 1px #1e2d44,0 16px 40px #000a;">${svg}</div>
            <div style="width:40px;height:40px;border-radius:50%;overflow:hidden;box-shadow:0 0 0 1px #1e2d44,0 12px 28px #000a;">${svg}</div>
            <span style="color:#6b7e99;font-size:13px;">Discord circular crop check</span>
          </div>
        </section>
        <section style="display:grid;place-items:center;">
          <div style="width:520px;height:520px;border-radius:28px;background:#111118;display:grid;place-items:center;box-shadow:inset 0 0 0 1px #1e1e2a;">
            <div style="width:420px;height:420px;">${svg}</div>
          </div>
        </section>
      </div>
    </body></html>`);
  await page.screenshot({ path: preview, fullPage: true });
  await page.close();
} finally {
  await browser.close();
}
console.log(out1024);
console.log(out512);
console.log(preview);
