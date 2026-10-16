#!/usr/bin/env node
// Zips dist/extension into dist/jai-viewer-<version>.zip for loading or publishing.
import { readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
const { version } = JSON.parse(readFileSync('dist/extension/manifest.json', 'utf8'));
const out = `dist/jai-viewer-${version}.zip`;
execFileSync('python3', ['-I', '-c', `import shutil,sys; shutil.make_archive(sys.argv[1][:-4], 'zip', 'dist/extension')`, out], { stdio: 'inherit' });
console.log('wrote', out);
