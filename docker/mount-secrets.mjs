// Emit secret references and source paths, never secret values. Also used by tests.
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export function secretPlan(config, directory) {
  const plan = [];
  for (const [kind, field, legacy, needs] of [
    ['couchdb', 'password', 'couchdb_password', () => true],
    ['couchdb', 'e2ee-passphrase', 'e2ee_passphrase', m => !!m.backend.e2ee],
    ['algolia', 'api-key', 'algolia_api_key', () => true],
  ]) {
    const mounts = (config.mounts ?? []).filter(m => m.backend.kind === kind && needs(m));
    const root = mounts.find(m => String(m.mountAt ?? '').replace(/^\/+|\/+$/g, '') === '');
    for (const mount of mounts) {
      if (!/^[a-z0-9][a-z0-9-]*$/.test(mount.id)) throw Error('invalid mount ID in secret configuration');
      let file = path.join(directory, `mount-${mount.id}-${field}`);
      if (!fs.existsSync(file)) {
        // Keep the old single/root credential working when another vault is added.
        if (mounts.length === 1 || mount === root) file = path.join(directory, legacy);
        else throw Error(`missing ${file}; provide a separate secret for mount '${mount.id}'`);
      }
      if (!fs.existsSync(file)) {
        if (mounts.length === 1) continue; // Preserve the existing degraded single-mount mode.
        throw Error(`missing ${file} for mount '${mount.id}'`);
      }
      let value;
      try { value = fs.readFileSync(file, 'utf8').split(/\r?\n/)[0]; }
      catch { throw Error(`cannot read ${file}; check the container user's file permissions`); }
      if (!value.trim()) throw Error(`${file} is empty`);
      if (/[\t\r\n]/.test(file)) throw Error('secret paths cannot contain tabs or newlines');
      plan.push([mount.id, field, file]);
    }
  }
  return plan;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
    for (const row of secretPlan(config, process.argv[3])) console.log(row.join('\t'));
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
