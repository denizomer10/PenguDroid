import { readFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = resolve(root, 'dist');

const read = (file) => readFile(resolve(dist, file), 'utf8');
const count = (haystack, needle) => haystack.split(needle).length - 1;

const failures = [];
const expect = (label, actual, expected) => {
  if (actual !== expected) failures.push(`${label}: expected ${expected}, found ${actual}`);
  console.log(`${actual === expected ? 'ok  ' : 'FAIL'} ${label}: ${actual}`);
};

const compat = await read('compatibility/index.html');
const docs = await read('docs/index.html');
const home = await read('index.html');

expect('audited scenario cards', count(compat, 'class="glass glass--soft scenario"'), 14);
expect('passed scenario pills', count(compat, 'pill--passed'), 7);
expect('partial scenario pills', count(compat, 'pill--partial'), 4);
expect('missing scenario pills', count(compat, 'pill--missing'), 3);
expect('observed missing Java signatures', count(compat, 'class="sig"'), 71);
expect('documentation cards', count(docs, 'class="glass glass--soft doc-card"'), 21);
expect('FAQ entries', count(docs, 'class="faq__answer"'), 8);
expect('home evidence cards', count(home, 'evidence-card sheen reveal'), 3);

const pages = {
  'index.html': home,
  'compatibility/index.html': compat,
  'docs/index.html': docs,
  'install/index.html': await read('install/index.html'),
  '404.html': await read('404.html'),
};
const banned = ['fonts.googleapis.com', 'fonts.gstatic.com', 'cdn.jsdelivr.net', 'unpkg.com', 'esm.sh'];
for (const [file, html] of Object.entries(pages)) {
  for (const host of banned) {
    if (html.includes(host)) failures.push(`${file} references external dependency ${host}`);
  }
  if (!html.includes('<html lang="en"')) failures.push(`${file} is missing lang="en"`);
}

if (failures.length) {
  console.error(`\n${failures.length} check(s) failed:`);
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}
console.log('\nAll content and dependency checks passed.');
