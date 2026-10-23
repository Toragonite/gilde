/**
 * A loopback static server for a built WebView bundle (the showcase build),
 * with the SPA fallback to index.html. Binds 127.0.0.1 on an ephemeral port;
 * serves only files inside `root` (a path that escapes it is a 404).
 */

import { createServer } from 'node:http';
import { createReadStream, existsSync, realpathSync, statSync } from 'node:fs';
import { extname, join, normalize, sep } from 'node:path';

const TYPES = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png', '.woff2': 'font/woff2', '.ico': 'image/x-icon', '.map': 'application/json' };

export function serveStatic(root) {
  const base = realpathSync(root);
  const server = createServer((req, res) => {
    let path;
    try {
      path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
    } catch {
      res.writeHead(400).end();
      return;
    }
    let file = normalize(join(base, path));
    if (file !== base && !file.startsWith(base + sep)) {
      res.writeHead(404).end();
      return;
    }
    if (!existsSync(file) || statSync(file).isDirectory()) {
      const index = join(file, 'index.html');
      file = existsSync(index) ? index : join(base, 'index.html');
    }
    if (!existsSync(file)) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream', 'cache-control': 'no-store' });
    createReadStream(file).pipe(res);
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => {
    const { port } = server.address();
    resolve({ url: `http://127.0.0.1:${port}`, close: () => new Promise((r) => server.close(() => r())) });
  }));
}
