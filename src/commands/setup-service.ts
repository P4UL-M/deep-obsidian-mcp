import path from "node:path";

import { ensureVaultPath } from "../vault.js";
import {
  buildServiceEndpoints,
  getDefaultServiceConfigPath,
  ensureHttpServiceConfig,
  normalizeServiceConfig,
  toPersistedServiceConfig,
  type ServiceConfigInput,
  type PersistedServiceConfig,
} from "../service.js";
import { assertCreatableDirectory, ensureWritableDirectory, pathExists, writeJsonFile } from "./shared.js";

export interface SetupServiceOptions {
  config: ServiceConfigInput;
  configFilePath?: string;
  dryRun?: boolean;
  overwrite?: boolean;
}

export interface SetupServiceResult {
  configFilePath: string;
  dryRun: boolean;
  written: boolean;
  persistedConfig: PersistedServiceConfig;
  endpoints: ReturnType<typeof buildServiceEndpoints>;
  messages: string[];
}

export async function setupService(options: SetupServiceOptions): Promise<SetupServiceResult> {
  const normalized = ensureHttpServiceConfig(normalizeServiceConfig(options.config));
  const vaultPath = await ensureVaultPath(normalized.vaultPath);
  const configFilePath = path.resolve(options.configFilePath ?? normalized.configFilePath ?? getDefaultServiceConfigPath());
  const persistedConfig = toPersistedServiceConfig({
    ...normalized,
    vaultPath,
  });

  const messages = [
    `vault: ${vaultPath}`,
    `config: ${configFilePath}`,
  ];

  if (options.dryRun) {
    await assertCreatableDirectory(normalized.indexDir);
    await assertCreatableDirectory(path.dirname(configFilePath));
    return {
      configFilePath,
      dryRun: true,
      written: false,
      persistedConfig,
      endpoints: buildServiceEndpoints(normalized),
      messages: [
        ...messages,
        `dry-run: config validated but not written`,
      ],
    };
  }

  await ensureWritableDirectory(normalized.indexDir);
  await ensureWritableDirectory(path.dirname(configFilePath));

  if (!options.overwrite && (await pathExists(configFilePath))) {
    throw new Error(`Config file already exists: ${configFilePath}`);
  }

  await writeJsonFile(configFilePath, persistedConfig);

  return {
    configFilePath,
    dryRun: false,
    written: true,
    persistedConfig,
    endpoints: buildServiceEndpoints(normalized),
    messages: [
      ...messages,
      `wrote config: ${configFilePath}`,
    ],
  };
}
