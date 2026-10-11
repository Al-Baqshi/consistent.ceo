#!/usr/bin/env node
// Serves desktop/noop on port 18999 so Tauri dev can load the loader page first (no extra npm deps).
const http = require('http');
const fs = require('fs');
const path = require('path');

const PORT = 18999;
const ROOT = path.join(__dirname, '..', 'noop');

const MIMES = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css',
  '.js': 'application/javascript',
  '.ico': 'image/x-icon',
  '.png': 'image/png',
};

const ROOT_NORM = path.resolve(ROOT);
const server = http.createServer((req, res) => {
  const p = path.resolve(ROOT, req.url === '/' ? 'index.html' : req.url.replace(/^\//, '').replace(/\.\./g, ''));
  const ext = path.extname(p);
  if (!p.startsWith(ROOT_NORM)) {
    res.writeHead(404);
    res.end('Not found');
    return;
  }
  fs.readFile(p, (err, data) => {
    if (err) {
      res.writeHead(404);
      res.end('Not found');
      return;
    }
    res.writeHead(200, { 'Content-Type': MIMES[ext] || 'application/octet-stream' });
    res.end(data);
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log('Loader server at http://127.0.0.1:' + PORT);
});
