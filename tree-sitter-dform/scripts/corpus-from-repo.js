// Regenerates test/corpus/repo/: one test per file the compiler reads (every
// .df at the repository's top level and under modules/, policies/,
// examples/ and providers/, and tests/syntax/ok/*.df, as
// tests/treesit_agreement.rs walks them). The expected trees are left
// empty; `tree-sitter test --update` fills them in. Run from
// tree-sitter-dform/: `npm run corpus` (or `node scripts/corpus-from-repo.js`).

import { mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { basename, join, relative } from 'node:path';

const repo = join(import.meta.dirname, '..', '..');
const out = join(import.meta.dirname, '..', 'test', 'corpus', 'repo');

function dfFiles(dir, recurse) {
  return readdirSync(dir).sort().flatMap(name => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return recurse ? dfFiles(path, true) : [];
    return name.endsWith('.df') ? [path] : [];
  });
}

const files = [
  ...dfFiles(repo, false),
  ...['modules', 'policies', 'examples', 'providers', 'crates/dform-mock/schemas', 'tests/syntax/ok']
    .flatMap(d => dfFiles(join(repo, d), true)),
];

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
for (const file of files) {
  const name = relative(repo, file);
  const src = readFileSync(file, 'utf8').replace(/\n+$/, '');
  if (/^(=+|-{3,})$/m.test(src)) throw new Error(`${name} has a line tree-sitter test reads as a separator`);
  const bar = '='.repeat(Math.max(name.length, 3));
  writeFileSync(
    join(out, name.replaceAll('/', '__').replace(/\.df$/, '.txt')),
    `${bar}\n${name}\n${bar}\n\n${src}\n\n---\n`,
  );
}
console.log(`${files.length} files -> ${relative(process.cwd(), out)}`);
