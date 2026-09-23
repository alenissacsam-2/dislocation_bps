// Development harness for the desk UI. Not shipped: tauri.conf.json bundles `ui/` only.
//
// Serves crates/desk/ui with a stand-in for the Tauri command bridge (dev/shim.js)
// injected ahead of app.js, so the window can be designed and screenshotted in an
// ordinary browser. Everything the shim returns is either read-only live data (the
// running bot's /api/equity and the tail of cb-bot.log, API keys masked) or a clearly
// fake fixture. It can start nothing, sign nothing, and write nothing.
//
//   node crates/desk/dev/server.cjs        then open http://127.0.0.1:5178/

const http = require('http');
const fs = require('fs');
const path = require('path');

const PORT = Number(process.env.PORT || 5178);
const UI = path.join(__dirname, '..', 'ui');
const DEV = __dirname;
const ROOT = path.join(__dirname, '..', '..', '..');
const TYPES = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8',
  '.cjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8',
  '.woff2': 'font/woff2', '.svg': 'image/svg+xml', '.png': 'image/png', '.json': 'application/json',
};

const mask = (s) => s
  .replace(/api-key=[^"&\s]*/g, 'api-key=***')
  .replace(/(https?:\/\/[^/\s"]+\/)[^\s"]+/g, '$1***');

function tail(file, lines) {
  try {
    const buf = fs.readFileSync(file);
    const start = Math.max(0, buf.length - 4 * 1024 * 1024);
    const all = buf.subarray(start).toString('utf8').split(/\r?\n/).filter(Boolean);
    return all.slice(-lines).map(mask);
  } catch { return []; }
}

http.createServer((req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  if (url.pathname === '/' || url.pathname === '/index.html') {
    const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8')
      .replace('<script src="app.js"></script>', '<script src="/dev/shim.js"></script>\n<script src="app.js"></script>');
    res.writeHead(200, { 'content-type': TYPES['.html'], 'cache-control': 'no-store' });
    return res.end(html);
  }
  if (url.pathname === '/dev/log') {
    const n = Number(url.searchParams.get('lines') || 400);
    res.writeHead(200, { 'content-type': 'application/json' });
    return res.end(JSON.stringify(tail(path.join(ROOT, 'cb-bot.log'), n)));
  }
  const base = url.pathname.startsWith('/dev/') ? DEV : UI;
  const rel = url.pathname.replace(/^\/dev\//, '/');
  const file = path.normalize(path.join(base, rel));
  if (!file.startsWith(base) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404); return res.end('not found');
  }
  res.writeHead(200, { 'content-type': TYPES[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
  fs.createReadStream(file).pipe(res);
}).listen(PORT, '127.0.0.1', () => console.log(`desk ui harness on http://127.0.0.1:${PORT}/`));
