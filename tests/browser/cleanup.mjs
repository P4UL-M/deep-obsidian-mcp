import { rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

export default async function cleanup() {
  const directory = process.env.DEEP_OBSIDIAN_BROWSER_FIREFOX_HOME;
  if (directory) {
    // macOS adds a deny-delete ACL to Library in this temporary app-data home.
    if (process.platform === 'darwin') await promisify(execFile)('/bin/chmod', ['-RN', directory]);
    await rm(directory, { recursive: true, force: true });
  }
}
