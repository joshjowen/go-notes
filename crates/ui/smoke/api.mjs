// A stand-in for the Go-Notes server: just enough API, in memory, with the
// same DTO shapes and the same If-Match semantics, so the real WebAssembly
// frontend can be driven through a full offline → sync → conflict cycle
// without Postgres.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const dist = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const types = {
  '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm',
  '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml',
  '.webmanifest': 'application/manifest+json',
};

export function createServer() {
  const notes = new Map(); // path -> markdown
  notes.set('Kitchen.md', '# Kitchen\n\nquotes are in\n');
  notes.set('Budget.md', '# Budget\n\n');

  const state = { reachable: true, signedIn: false, requests: [] };

  const hash = (text) => crypto.createHash('sha256').update(text).digest('hex').slice(0, 32);
  const stem = (p) => p.replace(/\.md$/, '').split('/').pop();

  const meta = (p) => ({
    path: p,
    title: stem(p),
    content_hash: hash(notes.get(p)),
    modified: new Date().toISOString(),
    size_bytes: Buffer.byteLength(notes.get(p)),
    tags: [],
  });

  const tree = () => ({
    kind: 'folder', name: '', path: '', collapsed: false,
    children: [...notes.keys()].sort().map((p) => ({
      kind: 'note', name: p.split('/').pop(), path: p, title: stem(p),
    })),
  });

  const json = (res, code, body) => {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(body));
  };

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://x');
    const p = decodeURIComponent(url.pathname);

    if (p.startsWith('/api/')) {
      state.requests.push(`${req.method} ${p}`);
      // "The server is not there" is a refused connection, not an error
      // status: the app treats a reply of any kind as the server being up.
      if (!state.reachable) {
        req.socket.destroy();
        return;
      }
    }

    let body = '';
    for await (const chunk of req) body += chunk;

    // --- identity ---------------------------------------------------------
    const me = { username: 'josh', display_name: 'josh', email: null, auth_provider: 'local' };
    if (p === '/api/auth/info') return json(res, 200, { local_enabled: true, oidc_button: null });
    if (p === '/api/auth/login') { state.signedIn = true; return json(res, 200, me); }
    if (p === '/api/me') {
      if (!state.signedIn) return json(res, 401, { code: 'unauthenticated', message: 'sign in' });
      return json(res, 200, me);
    }
    if (p === '/api/auth/logout') return json(res, 200, { redirect_to: null });

    // --- tree -------------------------------------------------------------
    if (p === '/api/tree') return json(res, 200, tree());
    if (p === '/api/folders/state') { res.writeHead(204).end(); return; }

    // --- notes ------------------------------------------------------------
    if (p.startsWith('/api/notes/')) {
      const notePath = p.slice('/api/notes/'.length);

      if (req.method === 'GET') {
        if (!notes.has(notePath)) return json(res, 404, { code: 'not_found', message: 'gone' });
        return json(res, 200, {
          meta: meta(notePath), markdown: notes.get(notePath), backlinks: [], outgoing: [],
        });
      }

      if (req.method === 'PUT') {
        const wanted = req.headers['if-match'];
        if (notes.has(notePath) && wanted !== hash(notes.get(notePath))) {
          return json(res, 409, {
            code: 'conflict',
            message: 'this note changed on disk',
            current_markdown: notes.get(notePath),
            current_hash: hash(notes.get(notePath)),
          });
        }
        notes.set(notePath, JSON.parse(body).markdown);
        return json(res, 200, { meta: meta(notePath) });
      }

      if (req.method === 'DELETE') { notes.delete(notePath); res.writeHead(204).end(); return; }
    }

    if (p === '/api/notes' && req.method === 'POST') {
      const { path: notePath, markdown } = JSON.parse(body);
      if (notes.has(notePath)) {
        return json(res, 409, { code: 'already_exists', message: `'${notePath}' already exists` });
      }
      notes.set(notePath, markdown ?? '');
      return json(res, 200, { meta: meta(notePath) });
    }

    // --- the rest ---------------------------------------------------------
    if (p.startsWith('/api/backlinks/')) return json(res, 200, []);
    if (p === '/api/quickswitch' || p === '/api/tagged') {
      return json(res, 200, [...notes.keys()].map((n) => ({ path: n, title: stem(n), exists: true })));
    }
    if (p === '/api/tags') return json(res, 200, []);
    if (p === '/api/search') return json(res, 200, { hits: [] });
    if (p === '/api/graph') return json(res, 200, { nodes: [], edges: [] });
    if (p.startsWith('/api/')) return json(res, 404, { code: 'not_found', message: 'no route' });

    // --- the frontend -----------------------------------------------------
    let file = path.join(dist, p);
    if (!fs.existsSync(file) || fs.statSync(file).isDirectory()) file = path.join(dist, 'index.html');
    res.writeHead(200, { 'content-type': types[path.extname(file)] || 'application/octet-stream' });
    fs.createReadStream(file).pipe(res);
  });

  return { server, notes, state, hash };
}
