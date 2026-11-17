import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { secretPlan } from '../docker/mount-secrets.mjs';

const mount = (id, mountAt, kind = 'couchdb', e2ee) => ({id, mountAt, backend:{kind, e2ee}});
function fixture(run) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'mount-secrets-'));
  try { run(directory, (name, value = 'test-only-password') => fs.writeFileSync(path.join(directory, name), value)); }
  finally { fs.rmSync(directory, { recursive:true, force:true }); }
}
test('legacy root plus private mount use distinct credentials across boots', () => fixture((directory, write) => {
  write('couchdb_password'); write('mount-laura-private-password', 'second-test-password');
  const config = {mounts:[mount('vault', '/'), mount('laura-private', 'Laura')]};
  for (let boot = 0; boot < 2; boot++) assert.deepEqual(secretPlan(config, directory), [
    ['vault','password',path.join(directory,'couchdb_password')],
    ['laura-private','password',path.join(directory,'mount-laura-private-password')],
  ]);
  fs.unlinkSync(path.join(directory,'mount-laura-private-password'));
  assert.throws(() => secretPlan(config,directory), /missing .*mount-laura-private-password/);
}));
test('per-mount overrides legacy and covers encryption and Algolia', () => fixture((directory, write) => {
  for (const name of ['couchdb_password','mount-vault-password','mount-vault-e2ee-passphrase','mount-wiki-api-key']) write(name);
  assert.deepEqual(secretPlan({mounts:[mount('vault','','couchdb',{}),mount('wiki','Wiki','algolia')]},directory).map(r=>path.basename(r[2])),[
    'mount-vault-password','mount-vault-e2ee-passphrase','mount-wiki-api-key',
  ]);
}));
test('never broadcasts legacy passwords to separate mounts or emits secret values', () => fixture((directory, write) => {
  write('couchdb_password','do-not-broadcast');
  assert.throws(()=>secretPlan({mounts:[mount('a','A'),mount('b','B')]},directory),/missing/);
  write('mount-a-password','');
  assert.throws(()=>secretPlan({mounts:[mount('a','A')]},directory),/empty/);
  write('mount-a-password','do-not-output');
  assert.ok(!JSON.stringify(secretPlan({mounts:[mount('a','A')]},directory)).includes('do-not-output'));
}));
